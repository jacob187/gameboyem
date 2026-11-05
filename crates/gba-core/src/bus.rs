//! The system bus: memory map, I/O dispatch, wait states and event dispatch.
//!
//! [`CpuBus`], [`Access`] and [`SwiResult`] are the **Contract** between the CPU
//! and everything else; the CPU is generic over `CpuBus`, so its tests can use a
//! flat-memory stub. [`Bus`] itself is owned by the system agent: the memory map
//! below is a placeholder (every access costs 1 cycle, little-endian byte
//! composition, no open bus, no WAITCNT) that P1-SYS replaces.

use crate::apu::Apu;
use crate::bios::Bios;
use crate::cart::Cart;
use crate::dma::Dma;
use crate::irq::Irq;
use crate::keypad::Keypad;
use crate::ppu::Ppu;
use crate::scheduler::{Event, Scheduler};
use crate::timer::Timers;

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
    /// Frames completed (incremented at VBlank start).
    pub frames: u64,
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
            frames: 0,
        };
        bus.ppu.start(&mut bus.sched);
        bus
    }

    /// Dispatch every event that is due. Called by the run loop after each CPU
    /// instruction, DMA unit or halt skip.
    pub fn process_events(&mut self) {
        while let Some((event, at)) = self.sched.pop_due() {
            match event {
                Event::PpuHBlank | Event::PpuLineEnd => {
                    let sig = self.ppu.on_event(event, at, &mut self.sched);
                    self.irq.if_ |= sig.irq;
                    if sig.frame_done {
                        self.frames += 1;
                    }
                    // P1-SYS: sig.hblank_dma / sig.vblank_dma -> self.dma.trigger(..)
                }
                Event::TimerOverflow(_) | Event::ApuSample | Event::DmaStart(_) => {
                    // P1-SYS / P2-APU.
                }
                Event::IrqLine => {}
            }
        }
        if self.halted && self.irq.wakes_halt() {
            self.halted = false;
        }
    }

    /// Read a byte with no side effects and no time cost (test and debug hook).
    pub fn peek8(&self, addr: u32) -> u8 {
        match addr >> 24 {
            0x00 => self.bios.read8(addr),
            0x02 => self.ewram[(addr & 0x3_FFFF) as usize],
            0x03 => self.iwram[(addr & 0x7FFF) as usize],
            0x04 => {
                let half = self.read_io16(addr & 0x3FE);
                (half >> ((addr & 1) * 8)) as u8
            }
            0x05 => self.ppu.palette[(addr & 0x3FF) as usize],
            0x06 => self.ppu.vram[vram_index(addr)],
            0x07 => self.ppu.oam[(addr & 0x3FF) as usize],
            0x08..=0x0D => self.cart.read_rom8(addr & 0x01FF_FFFF),
            0x0E | 0x0F => self.cart.read_save8(addr & 0xFFFF),
            _ => 0,
        }
    }

    fn poke8(&mut self, addr: u32, v: u8) {
        match addr >> 24 {
            0x02 => self.ewram[(addr & 0x3_FFFF) as usize] = v,
            0x03 => self.iwram[(addr & 0x7FFF) as usize] = v,
            0x04 => {
                let mask = 0xFFu16 << ((addr & 1) * 8);
                self.write_io16(addr & 0x3FE, u16::from(v) << ((addr & 1) * 8), mask);
            }
            0x05 => self.ppu.palette[(addr & 0x3FF) as usize] = v,
            0x06 => self.ppu.vram[vram_index(addr)] = v,
            0x07 => self.ppu.oam[(addr & 0x3FF) as usize] = v,
            0x0E | 0x0F => self.cart.write_save8(addr & 0xFFFF, v),
            _ => {}
        }
    }

    /// I/O read at offset `off` (even, 0..0x3FF from 0x0400_0000).
    fn read_io16(&self, off: u32) -> u16 {
        match off {
            0x000..=0x05F => self.ppu.read_io(off),
            0x130 => self.keypad.keyinput(),
            0x200 => self.irq.ie,
            0x202 => self.irq.if_,
            0x208 => u16::from(self.irq.ime),
            _ => 0,
        }
    }

    /// I/O write: `mask` selects the byte lanes written (0x00FF, 0xFF00, 0xFFFF).
    fn write_io16(&mut self, off: u32, v: u16, mask: u16) {
        match off {
            0x000..=0x05F => self.ppu.write_io(off, v, mask),
            0x200 => self.irq.ie = (self.irq.ie & !mask) | (v & mask),
            0x202 => self.irq.write_if(v & mask),
            0x208 => {
                if mask & 1 != 0 {
                    self.irq.ime = v & 1 != 0;
                }
            }
            // HALTCNT is the high byte of 0x300 (POSTFLG is the low byte).
            0x300 if mask & 0xFF00 != 0 => self.halted = true,
            _ => {}
        }
    }
}

/// VRAM is 96 KiB mirrored in 128 KiB steps; the top 32 KiB of each 128 KiB
/// window mirrors the 32 KiB OBJ area above it (0x1_8000..0x2_0000 -> 0x1_0000..).
fn vram_index(addr: u32) -> usize {
    let a = addr & 0x1_FFFF;
    (if a >= 0x1_8000 { a - 0x8000 } else { a }) as usize
}

impl CpuBus for Bus {
    fn read8(&mut self, addr: u32, _access: Access) -> u8 {
        self.sched.advance(1);
        self.peek8(addr)
    }
    fn read16(&mut self, addr: u32, _access: Access) -> u16 {
        self.sched.advance(1);
        let a = addr & !1;
        u16::from_le_bytes([self.peek8(a), self.peek8(a + 1)])
    }
    fn read32(&mut self, addr: u32, _access: Access) -> u32 {
        self.sched.advance(1);
        let a = addr & !3;
        u32::from_le_bytes([
            self.peek8(a),
            self.peek8(a + 1),
            self.peek8(a + 2),
            self.peek8(a + 3),
        ])
    }
    fn write8(&mut self, addr: u32, value: u8, _access: Access) {
        self.sched.advance(1);
        self.poke8(addr, value);
    }
    fn write16(&mut self, addr: u32, value: u16, _access: Access) {
        self.sched.advance(1);
        let a = addr & !1;
        for (i, b) in value.to_le_bytes().into_iter().enumerate() {
            self.poke8(a + i as u32, b);
        }
    }
    fn write32(&mut self, addr: u32, value: u32, _access: Access) {
        self.sched.advance(1);
        let a = addr & !3;
        for (i, b) in value.to_le_bytes().into_iter().enumerate() {
            self.poke8(a + i as u32, b);
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
    fn vram_mirrors_obj_area() {
        assert_eq!(vram_index(0x0601_7FFF), 0x1_7FFF);
        assert_eq!(vram_index(0x0601_8000), 0x1_0000);
        assert_eq!(vram_index(0x0602_0000), 0);
    }
}
