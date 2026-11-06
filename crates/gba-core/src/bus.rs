//! The system bus: wait-state accounting, event dispatch and the [`CpuBus`] impl.
//!
//! [`CpuBus`], [`Access`] and [`SwiResult`] are the **Contract** between the CPU
//! and everything else; the CPU is generic over `CpuBus`, so its tests can use a
//! flat-memory stub. [`Bus`] itself is owned by the system agent. The memory map
//! (region reads/writes) lives in `memory.rs`, the I/O registers in `io.rs`.
//!
//! # How time is charged
//!
//! Every `CpuBus` access first advances the master clock by [`Bus::cost`] (1 cycle
//! plus the region's wait states for that width and [`Access`] kind), then does its
//! effect, so a timer read sees the clock at the end of its own access. Events
//! that fall due are dispatched by the run loop after the instruction. Cartridge-ROM
//! costs come from [`WaitTable::rom_access`]; opcode fetches from ROM go through the
//! prefetch buffer ([`crate::prefetch`]) when WAITCNT bit 14 is set.

use crate::apu::Apu;
use crate::bios::Bios;
use crate::cart::Cart;
use crate::dma::{Dma, DmaTrigger};
use crate::io::WAITCNT_PREFETCH;
use crate::irq::Irq;
use crate::keypad::Keypad;
use crate::memory::rd;
use crate::ppu::Ppu;
use crate::prefetch::Prefetch;
use crate::scheduler::{Event, Scheduler};
use crate::state::{StateError, check, get_u16s, get_u32s, put_u16s, put_u32s, tag};
use crate::timer::Timers;
use emu_common::state::{StateReader, StateWriter};

/// Width of a bus access, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    B = 1,
    H = 2,
    W = 4,
}

/// Bus cycle kind (GBATEK "Memory Timings"). A sequential access is the one
/// directly after an access to the preceding address in the same region; the
/// CPU says which it is. Regions charge different wait states for each kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    NonSeq,
    Seq,
}

/// What an HLE BIOS did with a software interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwiResult {
    /// Not emulated: the CPU takes the SWI exception into BIOS code (vector 0x08).
    Unhandled,
    /// Done; registers updated. The CPU continues after the SWI instruction.
    Done,
    /// The call waits (Halt, IntrWait, VBlankIntrWait). The bus has halted the
    /// CPU; the CPU must leave PC on the SWI instruction so it runs again after
    /// the wake-up and the HLE call can re-check its condition.
    WaitThenRetry,
}

/// The CPU's view of the bus. **Contract.**
///
/// Addresses arrive as the CPU computed them; the bus aligns them down to the
/// access width (`addr & !1` / `addr & !3`). Rotating a misaligned load result is
/// the CPU's job. Every call charges its cycles to the master clock (1 + the
/// region's wait states for `access`).
pub trait CpuBus {
    fn read8(&mut self, addr: u32, access: Access) -> u8;
    fn read16(&mut self, addr: u32, access: Access) -> u16;
    fn read32(&mut self, addr: u32, access: Access) -> u32;
    fn write8(&mut self, addr: u32, value: u8, access: Access);
    fn write16(&mut self, addr: u32, value: u16, access: Access);
    fn write32(&mut self, addr: u32, value: u32, access: Access);

    /// An opcode fetch. Separate from data reads so the bus can track the BIOS
    /// open-bus/read-protection value and the cartridge prefetch buffer.
    fn fetch16(&mut self, addr: u32, access: Access) -> u16 {
        self.read16(addr, access)
    }
    /// See [`CpuBus::fetch16`].
    fn fetch32(&mut self, addr: u32, access: Access) -> u32 {
        self.read32(addr, access)
    }

    /// `cycles` internal (I) cycles: multiply steps, register-shift, LDR writeback.
    fn idle(&mut self, cycles: u32);

    /// The IRQ line (IME and IE & IF). The CPU also checks CPSR.I.
    fn irq_line(&self) -> bool;

    /// A software interrupt with comment `number` (ARM: bits 16..=23 of the
    /// opcode; Thumb: bits 0..=7). `r` holds r0..=r3 in and out. The default (a
    /// real BIOS) leaves it to the exception.
    fn swi(&mut self, number: u8, r: &mut [u32; 4]) -> SwiResult {
        let _ = (number, r);
        SwiResult::Unhandled
    }
}

/// Per-region access costs derived from WAITCNT, rebuilt on every WAITCNT write.
///
/// `rom[ws][seq][wide]` is the total cycle count (1 + waits) of a ROM access in
/// wait-state area `ws` (0x08/0x0A/0x0C), where `seq` is 0 for a non-sequential
/// and 1 for a sequential access, and `wide` is 0 for 8/16-bit and 1 for 32-bit
/// (two 16-bit accesses: N+S or S+S). GBATEK "GBA System Control", WAITCNT.
#[derive(Debug, Clone, Copy)]
pub struct WaitTable {
    pub rom: [[[u8; 2]; 2]; 3],
    /// SRAM / Flash (8-bit bus; N and S cost the same).
    pub sram: u8,
}

impl WaitTable {
    /// Cycles for a cartridge-ROM access at `addr`, 16-bit or (`wide`) 32-bit. A
    /// sequential access is only sequential if the CPU/DMA says so *and* it
    /// doesn't start a new 128 KiB page (the cart's address latch rolls over;
    /// GBATEK "GBA Cart ROM" / nba hw-test `128kb-boundary`). A word is two
    /// halfword accesses: N+S, or S+S.
    #[inline]
    pub fn rom_access(&self, addr: u32, wide: bool, access: Access) -> u64 {
        let ws = ((addr >> 25) & 3) as usize;
        let seq = access == Access::Seq && addr & 0x1_FFFF != 0;
        u64::from(self.rom[ws][usize::from(seq)][usize::from(wide)])
    }

    pub fn from_waitcnt(w: u16) -> Self {
        const N: [u8; 4] = [4, 3, 2, 8];
        const S: [[u8; 2]; 3] = [[2, 1], [4, 1], [8, 1]];
        let mut rom = [[[0; 2]; 2]; 3];
        for (ws, table) in rom.iter_mut().enumerate() {
            let shift = 2 + 3 * ws as u16;
            let n = 1 + N[usize::from((w >> shift) & 3)];
            let s = 1 + S[ws][usize::from((w >> (shift + 2)) & 1)];
            *table = [[n, n + s], [s, 2 * s]];
        }
        Self {
            rom,
            sram: 1 + N[usize::from(w & 3)],
        }
    }
}

/// Every component except the CPU, plus the memory they share.
pub struct Bus {
    pub sched: Scheduler,
    pub irq: Irq,
    pub ppu: Ppu,
    pub dma: Dma,
    pub timers: Timers,
    pub keypad: Keypad,
    pub cart: Cart,
    pub apu: Apu,
    pub bios: Bios,
    /// 256 KiB on-board work RAM (0x0200_0000, mirrored every 256 KiB).
    pub ewram: Box<[u8]>,
    /// 32 KiB on-chip work RAM (0x0300_0000, mirrored every 32 KiB).
    pub iwram: Box<[u8]>,
    /// Set by HALTCNT or an HLE wait; cleared when `irq.wakes_halt()`.
    pub halted: bool,
    /// HALTCNT bit 7 (stop): wakes only on keypad / cartridge / serial IRQs.
    pub stopped: bool,
    /// Frames completed (incremented at VBlank start).
    pub frames: u64,
    /// WAITCNT (0x0400_0204) and the costs derived from it.
    pub waitcnt: u16,
    pub waits: WaitTable,
    /// The GamePak prefetch buffer (WAITCNT bit 14, DECISIONS #70).
    pub prefetch: Prefetch,
    /// The last value the CPU fetched, replicated to 32 bits for a halfword
    /// fetch: what unmapped regions read as (open bus, DECISIONS #52).
    pub open_bus: u32,
    /// The last word fetched from the BIOS: what BIOS reads return while the CPU
    /// executes outside the BIOS (read protection).
    pub bios_latch: u32,
    /// The most recent opcode fetch came from the BIOS region.
    pub fetching_bios: bool,
    /// POSTFLG (0x0400_0300, low byte).
    pub postflg: u8,
    /// P2-BIOS: an HLE IntrWait/VBlankIntrWait is waiting, with the interrupt
    /// flags it waits for. Save-state data (DECISIONS #65).
    pub intr_wait: Option<u16>,
    /// FIFO A/B words being assembled from halfword/byte writes.
    pub fifo_latch: [u32; 2],
    /// Serial/JOY registers 0x120..0x160: stored, not emulated.
    pub sio_regs: [u16; 0x20],
}

impl Bus {
    pub fn new(cart: Cart, bios: Bios) -> Self {
        let mut bus = Self {
            sched: Scheduler::new(),
            irq: Irq::default(),
            ppu: Ppu::new(),
            dma: Dma::default(),
            timers: Timers::default(),
            keypad: Keypad::default(),
            cart,
            apu: Apu::default(),
            bios,
            ewram: vec![0; 256 * 1024].into_boxed_slice(),
            iwram: vec![0; 32 * 1024].into_boxed_slice(),
            halted: false,
            stopped: false,
            frames: 0,
            waitcnt: 0,
            waits: WaitTable::from_waitcnt(0),
            prefetch: Prefetch::default(),
            open_bus: 0,
            // What the BIOS leaves behind after startup (GBATEK "BIOS Memory
            // Protection"); P2-BIOS refines it.
            bios_latch: 0xE129_F000,
            fetching_bios: false,
            postflg: 0,
            intr_wait: None,
            fifo_latch: [0; 2],
            sio_regs: [0; 0x20],
        };
        bus.ppu.start(&mut bus.sched);
        // P2-APU: the sound clock pump (see `Apu::advance_to`).
        bus.sched
            .schedule(Event::ApuSample, crate::apu::PUMP_CYCLES);
        bus
    }

    /// Every component but the CPU, in the section order of DECISIONS #73. The
    /// scheduler goes first and is restored verbatim; nothing is re-derived from
    /// the registers afterwards except the wait-state table (a pure function of
    /// WAITCNT) and the timers' FIFO-length mirror (a copy of the APU's lengths).
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.u8(tag::SCHEDULER);
        self.sched.save_state(w);
        w.u8(tag::BUS);
        self.save_misc(w);
        w.u8(tag::IRQ);
        self.irq.save_state(w);
        w.u8(tag::TIMERS);
        self.timers.save_state(w);
        w.u8(tag::DMA);
        self.dma.save_state(w);
        w.u8(tag::PPU);
        self.ppu.save_state(w);
        w.u8(tag::APU);
        self.apu.save_state(w);
        w.u8(tag::KEYPAD);
        self.keypad.save_state(w);
        w.u8(tag::PREFETCH);
        self.prefetch.save_state(w);
        w.u8(tag::CART);
        self.cart.save_state(w);
    }

    /// The bus's own fields: both work RAMs and the flags and latches.
    pub(crate) fn save_misc(&self, w: &mut StateWriter) {
        w.bytes(&self.ewram);
        w.bytes(&self.iwram);
        w.bool(self.halted);
        w.bool(self.stopped);
        w.u64(self.frames);
        w.u16(self.waitcnt);
        w.u32(self.open_bus);
        w.u32(self.bios_latch);
        w.bool(self.fetching_bios);
        w.u8(self.postflg);
        w.bool(self.intr_wait.is_some());
        w.u16(self.intr_wait.unwrap_or(0));
        put_u32s(w, &self.fifo_latch);
        put_u16s(w, &self.sio_regs);
    }

    /// Restore what [`Bus::save_state`] wrote. Components validate their own
    /// fields; the checks here are the ones that relate two components, which is
    /// why they run after every section is in.
    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.tag(tag::SCHEDULER)?;
        self.sched.load_state(r)?;
        r.tag(tag::BUS)?;
        r.bytes_into(&mut self.ewram)?;
        r.bytes_into(&mut self.iwram)?;
        let (halted, stopped) = (r.bool()?, r.bool()?);
        let frames = r.u64()?;
        let waitcnt = r.u16()?;
        let (open_bus, bios_latch) = (r.u32()?, r.u32()?);
        let fetching_bios = r.bool()?;
        let postflg = r.u8()?;
        let (waiting, wanted) = (r.bool()?, r.u16()?);
        let fifo_latch = get_u32s::<2>(r)?;
        let mut sio_regs = [0; 0x20];
        get_u16s(r, &mut sio_regs)?;
        check(!stopped || halted, "stopped without halting")?;
        check(waitcnt & 0x8000 == 0, "WAITCNT")?;
        check(postflg <= 1, "POSTFLG")?;
        check(waiting || wanted == 0, "IntrWait flags without a wait")?;
        check(frames < crate::state::MAX_CLOCK, "frame count")?;
        self.halted = halted;
        self.stopped = stopped;
        self.frames = frames;
        self.waitcnt = waitcnt;
        self.waits = WaitTable::from_waitcnt(waitcnt);
        self.open_bus = open_bus;
        self.bios_latch = bios_latch;
        self.fetching_bios = fetching_bios;
        self.postflg = postflg;
        self.intr_wait = waiting.then_some(wanted);
        self.fifo_latch = fifo_latch;
        self.sio_regs = sio_regs;
        r.tag(tag::IRQ)?;
        self.irq.load_state(r)?;
        r.tag(tag::TIMERS)?;
        self.timers.load_state(r)?;
        r.tag(tag::DMA)?;
        self.dma.load_state(r)?;
        r.tag(tag::PPU)?;
        self.ppu.load_state(r)?;
        let now = self.sched.now();
        r.tag(tag::APU)?;
        self.apu.load_state(r, now)?;
        r.tag(tag::KEYPAD)?;
        self.keypad.load_state(r)?;
        r.tag(tag::PREFETCH)?;
        self.prefetch.load_state(r, now)?;
        r.tag(tag::CART)?;
        self.cart.load_state(r, now)?;

        // Relations between components.
        self.timers.fifo_len = self.apu.fifo_lens();
        self.timers.check_events(now, &self.sched)?;
        check(
            self.prefetch.enabled == (waitcnt & WAITCNT_PREFETCH != 0),
            "prefetch enable disagrees with WAITCNT",
        )
    }

    /// Dispatch every event that is due. Called by the run loop after each CPU
    /// instruction, DMA unit or halt skip.
    pub fn process_events(&mut self) {
        while let Some((event, at)) = self.sched.pop_due() {
            match event {
                Event::PpuFirstLine | Event::PpuRender | Event::PpuHBlank | Event::PpuLineEnd => {
                    let sig = self.ppu.on_event(event, at, &mut self.sched);
                    self.irq.if_ |= sig.irq;
                    if sig.frame_done {
                        self.frames += 1;
                    }
                    if sig.hblank_dma {
                        self.dma.trigger(DmaTrigger::HBlank);
                    }
                    if sig.vblank_dma {
                        self.dma.trigger(DmaTrigger::VBlank);
                    }
                    if event == Event::PpuLineEnd {
                        // Video capture (DMA3 special) runs on lines 2..=161 and
                        // stops at 162; the PPU has no signal for it, so read the
                        // line it just advanced to (GBATEK "Video Capture Mode").
                        match self.ppu.read_io(0x006) {
                            2..=161 => self.dma.trigger(DmaTrigger::VideoCapture),
                            162 => self.dma.stop_video_capture(),
                            _ => {}
                        }
                    }
                }
                Event::TimerOverflow(n) => self.timer_overflow(usize::from(n), at),
                // P2-APU: bring the APU up to date and re-arm the pump from its
                // own timestamp. The APU is exact without it (it also syncs on
                // register writes, FIFO timer overflows and drains); the pump just
                // bounds how stale its flags and buffered samples can get.
                Event::ApuSample => {
                    self.apu.advance_to(at);
                    self.sched
                        .schedule_at(Event::ApuSample, at + crate::apu::PUMP_CYCLES);
                }
                // DMA and IRQ delays are folded into the run loop, so this never
                // fires yet.
                Event::DmaStart(_) => {}
                Event::IrqLine => {}
            }
        }
        self.check_wake();
    }

    /// Dispatch the events that are due before the CPU touches video state, so a
    /// store lands on the right side of the PPU's render (cycle 960) and line end,
    /// and a status read sees the current line, instead of being up to an
    /// instruction early or late. A store to a timer register does the same: an
    /// overflow that fell earlier in the instruction must fire (IRQ, cascade, FIFO
    /// pop) before the write stops, re-prescales or reloads the timer (deep-review
    /// #13, #15). Only the CPU's own accesses do this; a DMA unit is never
    /// interrupted by event dispatch.
    #[inline]
    fn sync_ppu_write(&mut self, addr: u32) {
        let video = match addr >> 24 {
            0x04 => matches!(addr & 0x00FF_FFFF, 0..=0x5F | 0x100..=0x10F),
            0x05..=0x07 => true,
            _ => false,
        };
        if video {
            self.catch_up_events();
        }
    }

    /// See [`Bus::sync_ppu_write`]; reads matter for DISPSTAT and VCOUNT only.
    #[inline]
    fn sync_ppu_read(&mut self, addr: u32) {
        if addr >> 24 == 0x04 && addr & 0x00FF_FFFF < 0x60 {
            self.catch_up_events();
        }
    }

    #[inline]
    fn catch_up_events(&mut self) {
        let now = self.sched.now();
        if self.sched.next_at().is_some_and(|t| t <= now) {
            self.process_events();
        }
    }

    /// End a halt (or stop) if its wake condition holds. Called after events and
    /// whenever something outside the run loop raises an interrupt.
    pub fn check_wake(&mut self) {
        if !self.halted {
            return;
        }
        let wake = if self.stopped {
            // Stop wakes on the keypad, cartridge and serial interrupts only.
            self.irq.ie & self.irq.if_ & STOP_WAKE_MASK != 0
        } else {
            self.irq.wakes_halt()
        };
        if wake {
            self.halted = false;
            self.stopped = false;
        }
    }

    /// Cycles for one access of `w` to `addr` (1 + wait states). Pure, so DMA and
    /// the CPU share it.
    #[inline]
    pub fn cost(&self, addr: u32, w: Width, access: Access) -> u64 {
        match addr >> 24 {
            // EWRAM: 16-bit bus, 2 wait states; a word is two accesses.
            0x02 => {
                if w == Width::W {
                    6
                } else {
                    3
                }
            }
            // Palette and VRAM: 16-bit bus, no waits (the PPU contention penalty
            // is not modelled, DECISIONS #71); a word is two accesses.
            0x05 | 0x06 => {
                if w == Width::W {
                    2
                } else {
                    1
                }
            }
            0x08..=0x0D => self.rom_cost(addr, w, access),
            0x0E | 0x0F => u64::from(self.waits.sram),
            // BIOS, IWRAM, I/O, OAM (32-bit buses), and open bus.
            _ => 1,
        }
    }

    /// Cost of a cartridge ROM access without the prefetch buffer: the
    /// [`WaitTable`] entry (see [`WaitTable::rom_access`]). Pure, so the HLE BIOS
    /// can use it for its bookkeeping; the CPU and DMA go through
    /// [`Bus::access_cost`] and [`Bus::rom_fetch_cost`], which also keep the prefetch
    /// buffer's state.
    #[inline]
    pub fn rom_cost(&self, addr: u32, w: Width, access: Access) -> u64 {
        self.waits.rom_access(addr, w == Width::W, access)
    }

    /// Cost of a (non-fetch) access, updating the prefetch buffer when the access
    /// uses the cartridge bus. Used by the CPU and DMA.
    #[inline]
    pub fn access_cost(&mut self, addr: u32, w: Width, access: Access) -> u64 {
        let c = self.cost(addr, w, access);
        if self.prefetch.enabled && (0x08..=0x0D).contains(&(addr >> 24)) {
            self.prefetch.data_access(self.sched.now(), &self.waits, c);
        }
        c
    }

    /// Open-bus value for an access of `w` at `addr` (the CPU's last fetch).
    #[inline]
    pub fn open_bus_value(&self, addr: u32, w: Width) -> u32 {
        match w {
            Width::B => self.open_bus >> ((addr & 3) * 8) & 0xFF,
            Width::H => self.open_bus >> ((addr & 2) * 8) & 0xFFFF,
            Width::W => self.open_bus,
        }
    }

    /// An opcode fetch is about to read `addr`: from now on the CPU executes in
    /// the BIOS or not, which decides whether BIOS reads are protected. Done
    /// before the read so a fetch from the BIOS itself sees the real contents.
    #[inline]
    fn begin_fetch(&mut self, addr: u32) {
        self.fetching_bios = addr < crate::BIOS_SIZE as u32;
    }

    /// Cost of a cartridge-ROM opcode fetch: the only access the prefetch buffer
    /// serves (`Prefetch::fetch`, DECISIONS #70).
    #[inline]
    fn rom_fetch_cost(&mut self, addr: u32, w: Width, access: Access) -> u64 {
        if self.prefetch.enabled {
            self.prefetch
                .fetch(self.sched.now(), &self.waits, addr, w == Width::W, access)
        } else {
            self.rom_cost(addr, w, access)
        }
    }

    /// Record an opcode fetch for open bus and BIOS read protection.
    #[inline]
    fn note_fetch(&mut self, addr: u32, value: u32, w: Width) {
        self.open_bus = if w == Width::H {
            value & 0xFFFF | value << 16
        } else {
            value
        };
        if self.fetching_bios {
            // The latch holds a full word; a halfword fetch latches its word.
            let a = addr & !3;
            self.bios_latch = u32::from_le_bytes([
                self.bios.read8(a),
                self.bios.read8(a + 1),
                self.bios.read8(a + 2),
                self.bios.read8(a + 3),
            ]);
        }
    }
}

/// IF bits that wake a stopped CPU: serial (7), keypad (12), game pak (13).
const STOP_WAKE_MASK: u16 = 1 << 7 | 1 << 12 | 1 << 13;

impl CpuBus for Bus {
    #[inline]
    fn read8(&mut self, addr: u32, access: Access) -> u8 {
        let c = self.access_cost(addr, Width::B, access);
        self.sched.advance(c);
        self.sync_ppu_read(addr);
        self.read_value(addr, Width::B) as u8
    }
    #[inline]
    fn read16(&mut self, addr: u32, access: Access) -> u16 {
        let c = self.access_cost(addr, Width::H, access);
        self.sched.advance(c);
        self.sync_ppu_read(addr);
        self.read_value(addr, Width::H) as u16
    }
    #[inline]
    fn read32(&mut self, addr: u32, access: Access) -> u32 {
        let c = self.access_cost(addr, Width::W, access);
        self.sched.advance(c);
        self.sync_ppu_read(addr);
        self.read_value(addr, Width::W)
    }
    #[inline]
    fn write8(&mut self, addr: u32, value: u8, access: Access) {
        let c = self.access_cost(addr, Width::B, access);
        self.sched.advance(c);
        self.sync_ppu_write(addr);
        self.write_value(addr, u32::from(value), Width::B);
    }
    #[inline]
    fn write16(&mut self, addr: u32, value: u16, access: Access) {
        let c = self.access_cost(addr, Width::H, access);
        self.sched.advance(c);
        self.sync_ppu_write(addr);
        self.write_value(addr, u32::from(value), Width::H);
    }
    #[inline]
    fn write32(&mut self, addr: u32, value: u32, access: Access) {
        let c = self.access_cost(addr, Width::W, access);
        self.sched.advance(c);
        self.sync_ppu_write(addr);
        self.write_value(addr, value, Width::W);
    }
    #[inline]
    fn fetch16(&mut self, addr: u32, access: Access) -> u16 {
        // Per-region fast paths for the regions code runs from. Each does what the
        // general path does: the BIOS-fetch flag is cleared (no BIOS latch update
        // is needed), the open-bus value is latched and the clock charged.
        match addr >> 24 {
            0x08..=0x0D => {
                self.fetching_bios = false;
                let c = self.rom_fetch_cost(addr, Width::H, access);
                self.sched.advance(c);
                let v = self.read_rom(addr, Width::H);
                self.open_bus = v | v << 16;
                v as u16
            }
            0x03 => {
                self.fetching_bios = false;
                self.sched.advance(1);
                let v = rd(&self.iwram, addr as usize & 0x7FFF, Width::H);
                self.open_bus = v | v << 16;
                v as u16
            }
            0x02 => {
                self.fetching_bios = false;
                self.sched.advance(3);
                let v = rd(&self.ewram, addr as usize & 0x3_FFFF, Width::H);
                self.open_bus = v | v << 16;
                v as u16
            }
            _ => {
                self.begin_fetch(addr);
                self.sched.advance(self.cost(addr, Width::H, access));
                let v = self.read_region(addr, Width::H, false);
                self.note_fetch(addr, v, Width::H);
                v as u16
            }
        }
    }
    #[inline]
    fn fetch32(&mut self, addr: u32, access: Access) -> u32 {
        match addr >> 24 {
            0x08..=0x0D => {
                self.fetching_bios = false;
                let c = self.rom_fetch_cost(addr, Width::W, access);
                self.sched.advance(c);
                let v = self.read_rom(addr, Width::W);
                self.open_bus = v;
                v
            }
            0x03 => {
                self.fetching_bios = false;
                self.sched.advance(1);
                let v = rd(&self.iwram, addr as usize & 0x7FFF, Width::W);
                self.open_bus = v;
                v
            }
            0x02 => {
                self.fetching_bios = false;
                self.sched.advance(6);
                let v = rd(&self.ewram, addr as usize & 0x3_FFFF, Width::W);
                self.open_bus = v;
                v
            }
            _ => {
                self.begin_fetch(addr);
                self.sched.advance(self.cost(addr, Width::W, access));
                let v = self.read_region(addr, Width::W, false);
                self.note_fetch(addr, v, Width::W);
                v
            }
        }
    }
    fn idle(&mut self, cycles: u32) {
        self.sched.advance(u64::from(cycles));
    }
    fn irq_line(&self) -> bool {
        self.irq.line()
    }
    fn swi(&mut self, number: u8, r: &mut [u32; 4]) -> SwiResult {
        crate::bios::swi(self, number, r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cpu_store_after_the_render_point_lands_on_the_next_line() {
        // The store's own access crosses cycle 960, so the line is drawn first
        // even though the run loop would only dispatch the event after the
        // instruction (DECISIONS #71).
        const PAL: u32 = 0x0500_0000;
        let mut bus = Bus::new(Cart::from_rom(vec![0; 0x400]).unwrap(), Bios::Hle);
        bus.write16(PAL, 0x001F, Access::NonSeq); // cycle 1
        bus.idle(957); // cycle 958
        bus.write16(PAL, 0x03E0, Access::NonSeq); // completes at 959: line 0
        bus.write16(PAL, 0x7C00, Access::NonSeq); // completes at 960: line 1
        while bus.frames == 0 {
            bus.idle(100);
            bus.process_events();
        }
        let fb = bus.ppu.framebuffer();
        assert_eq!(fb[0], 0x03E0);
        assert_eq!(fb[crate::SCREEN_WIDTH], 0x7C00);
    }

    #[test]
    fn dispstat_and_vcount_reads_see_events_that_are_due() {
        let mut bus = Bus::new(Cart::from_rom(vec![0; 0x400]).unwrap(), Bios::Hle);
        bus.idle(crate::CYCLES_PER_LINE - 2);
        // This read ends one cycle short of line 1: no event due yet.
        assert_eq!(bus.read16(0x0400_0006, Access::NonSeq), 0);
        // The read itself costs a cycle and crosses the line end.
        assert_eq!(bus.read16(0x0400_0006, Access::NonSeq), 1);
    }
}
