//! The public emulator facade and the run loop. Owned by P1-SYS.

use crate::bios::Bios;
use crate::bus::Bus;
use crate::cart::{Cart, CartError, CartOptions, RtcMode, RtcTime, SaveChoice, SaveKind};
use crate::cpu::{Cpu, Registers};
use crate::keypad::Button;
use crate::state::{self, StateError, check, tag};
use crate::{CYCLES_PER_FRAME, Framebuffer, MAX_STATE_BYTES};
use emu_common::state::{StateReader, StateWriter};

/// The clock start the host gave [`Gba::set_rtc`], as a state stores it: a flag
/// and six fixed-width fields (zero when absent). It is host configuration that a
/// recording's header reads back, so a loaded machine must agree with the state
/// about it.
fn save_rtc_start(w: &mut StateWriter, t: Option<RtcTime>) {
    w.bool(t.is_some());
    w.u16(t.map_or(0, |t| t.year()));
    for v in t.map_or([0; 5], |t| {
        [t.month(), t.day(), t.hour(), t.minute(), t.second()]
    }) {
        w.u8(v);
    }
}

fn load_rtc_start(r: &mut StateReader) -> Result<Option<RtcTime>, StateError> {
    let given = r.bool()?;
    let year = r.u16()?;
    let mut f = [0u8; 5];
    for v in &mut f {
        *v = r.u8()?;
    }
    if !given {
        check(
            year == 0 && f == [0; 5],
            "clock start fields without a clock start",
        )?;
        return Ok(None);
    }
    RtcTime::new(year, f[0], f[1], f[2], f[3], f[4])
        .map(Some)
        .map_err(|_| StateError::Invalid("clock start"))
}

/// A complete Game Boy Advance: CPU + bus (which owns every other component).
pub struct Gba {
    cpu: Cpu,
    bus: Bus,
    /// FNV-1a of the ROM image; ties save states and recordings to their ROM.
    rom_hash: u64,
    /// What the host chose besides the ROM (`--save-type`, `--rtc`). Part of a
    /// state's and a recording's header: they pick the backup chip and the clock.
    options: CartOptions,
    /// FNV-1a of the real BIOS dump, or 0 for the built-in HLE BIOS.
    bios_hash: u64,
    /// The time the host started the cartridge clock at ([`Gba::set_rtc`]); a
    /// recording stores it so a replay starts the clock the same way.
    rtc_start: Option<RtcTime>,
}

impl Gba {
    /// Power on with `rom` inserted. With `bios = None` the built-in HLE BIOS is
    /// used and the machine starts in the post-BIOS state at the cartridge entry
    /// point; with a real 16 KiB dump it boots through the BIOS from address 0.
    /// The host validates the dump's size (and may check its hash).
    pub fn new(rom: Vec<u8>, bios: Option<Box<[u8]>>) -> Result<Self, CartError> {
        Self::with_options(rom, bios, CartOptions::default())
    }

    /// [`Gba::new`] with the host's cartridge choices: the save type
    /// (`--save-type`) and whether the cartridge has a clock (`--rtc`).
    pub fn with_options(
        rom: Vec<u8>,
        bios: Option<Box<[u8]>>,
        options: CartOptions,
    ) -> Result<Self, CartError> {
        let rom_hash = crate::fnv1a(&rom);
        let cart = Cart::with_options(rom, options)?;
        let (cpu, bios) = match bios {
            Some(data) => (Cpu::reset(), Bios::Real(data)),
            None => (Cpu::post_bios(), Bios::Hle),
        };
        let bios_hash = match &bios {
            Bios::Hle => 0,
            Bios::Real(data) => crate::fnv1a(data),
        };
        let hle = matches!(bios, Bios::Hle);
        let mut bus = Bus::new(cart, bios);
        if hle {
            // P2-BIOS: the I/O and IWRAM state the BIOS leaves (POSTFLG, SOUNDBIAS, the zeroed top of IWRAM).
            crate::bios::post_boot(&mut bus);
        }
        Ok(Self {
            cpu,
            bus,
            rom_hash,
            options,
            bios_hash,
            rtc_start: None,
        })
    }

    /// Run until the next VBlank starts (one frame), with a two-frame safety cap.
    pub fn step_frame(&mut self) {
        let start = self.bus.frames;
        let cap = self.bus.sched.now() + 2 * u64::from(CYCLES_PER_FRAME);
        while self.bus.frames == start && self.bus.sched.now() < cap {
            if !self.step_once(false) {
                break;
            }
        }
    }

    /// One step of the run loop: an active DMA (which holds the bus, even while
    /// the CPU is halted; `single_unit` moves just one unit, otherwise it runs
    /// until it finishes or an event is due), else the clock jumps to the next
    /// event while halted, else one CPU instruction. Due events are dispatched
    /// afterwards. Returns false if the machine is halted with nothing scheduled
    /// to wake it.
    fn step_once(&mut self, single_unit: bool) -> bool {
        if self.bus.dma.active_channel().is_some() {
            if single_unit {
                self.bus.run_dma_unit();
            } else {
                self.bus.run_dma();
            }
        } else if self.bus.halted {
            if !self.bus.sched.skip_to_next() {
                return false;
            }
        } else {
            self.cpu.step(&mut self.bus);
        }
        // Dispatch only when something is due, or while halted (the wake check at
        // the end of `process_events` is what ends a halt). Most instructions
        // cross no event, and the call is not free (P3-TIMING).
        let now = self.bus.sched.now();
        if self.bus.halted || self.bus.sched.next_at().is_some_and(|t| t <= now) {
            self.bus.process_events();
        }
        true
    }

    /// Advance the machine by one step and dispatch due events (test hook), so a
    /// loop of these always makes time pass and never spins:
    /// * if a DMA is active, run one DMA unit (the CPU is paused meanwhile);
    /// * else if the CPU is halted, skip the clock to the next event;
    /// * else execute one CPU instruction.
    pub fn step_instruction(&mut self) {
        self.step_once(true);
    }

    pub fn framebuffer(&self) -> &Framebuffer {
        self.bus.ppu.framebuffer()
    }

    pub fn set_button(&mut self, b: Button, pressed: bool) {
        self.bus.set_button(b, pressed);
    }

    /// Append the audio produced since the last call (interleaved stereo f32,
    /// see `Apu::drain_samples`).
    pub fn drain_audio_into(&mut self, out: &mut Vec<f32>) {
        // P2-APU: run the APU up to the clock first, so the audio covers exactly
        // the emulated time elapsed (signature and contract unchanged).
        self.bus.apu.advance_to(self.bus.sched.now());
        self.bus.apu.drain_samples(out);
    }

    pub fn audio_sample_rate(&self) -> u32 {
        self.bus.apu.sample_rate()
    }

    pub fn set_audio_sample_rate(&mut self, hz: u32) {
        // P2-APU: cycles already elapsed are accounted at the
        // old rate; the new one applies from now on.
        self.bus.apu.advance_to(self.bus.sched.now());
        self.bus.apu.set_sample_rate(hz);
    }

    /// The cartridge title from the header.
    pub fn title(&self) -> String {
        self.bus.cart.title()
    }

    pub fn rom_hash(&self) -> u64 {
        self.rom_hash
    }

    /// CPU registers (test hook).
    pub fn cpu_registers(&self) -> Registers {
        self.cpu.registers()
    }

    /// Read a byte with no side effects (test hook).
    pub fn peek8(&self, addr: u32) -> u8 {
        self.bus.peek8(addr)
    }

    /// Read a halfword (`addr & !1`) with no side effects (test hook).
    pub fn peek16(&self, addr: u32) -> u16 {
        self.bus.peek16(addr)
    }

    /// Read a word (`addr & !3`) with no side effects (test hook).
    pub fn peek32(&self, addr: u32) -> u32 {
        self.bus.peek32(addr)
    }

    /// The backup chip's contents alone: SRAM, Flash, or EEPROM (`None` without a
    /// backup chip, or for an EEPROM whose size is still unknown). A host writes
    /// [`Gba::battery_save`] to a `.sav` file, which adds the RTC footer.
    pub fn battery_ram(&self) -> Option<&[u8]> {
        self.bus.cart.battery_ram()
    }

    /// The backup chip the ROM's library string names, `SaveKind::None` without
    /// one. Unlike [`Gba::battery_ram`] it is known from power-on, even for an
    /// EEPROM whose size is not.
    pub fn save_kind(&self) -> SaveKind {
        self.bus.cart.save_kind()
    }

    /// Counter that advances only when the game changes save data (or a save is
    /// loaded), so a host can skip rewriting an unchanged `.sav`.
    pub fn battery_generation(&self) -> u64 {
        self.bus.cart.battery_generation()
    }

    /// The `.sav` image: [`Gba::battery_ram`] followed, once the game has used the
    /// cartridge's clock (a valid command reached it) or a save with a footer
    /// was loaded, by the 24-byte RTC footer.
    /// The clock in it is the emulated time now; no host time
    /// is stored.
    pub fn battery_save(&self) -> Option<Vec<u8>> {
        self.bus.cart.battery_save(self.bus.sched.now())
    }

    /// Restore a `.sav` image. The size must match the backup chip (an EEPROM
    /// accepts 512 or 8192 bytes unless the save type was forced), optionally
    /// followed by an RTC footer. A footer that is present but invalid rejects
    /// the file; a file without one loads and leaves the clock alone.
    pub fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), CartError> {
        self.bus
            .cart
            .load_battery_ram_at(data, self.bus.sched.now())
    }

    /// Set the cartridge clock to `t`, a time the host read from its own clock
    /// (the one place real time enters; the core reads no host clock, so a run stays a function of ROM, input and the
    /// starting state).
    /// Call it before loading a save: a save's own footer replaces it. Also
    /// clears the clock's power-loss flag. It does not add a footer to the save
    /// by itself; that waits for the game to use the clock.
    pub fn set_rtc(&mut self, t: RtcTime) {
        self.rtc_start = Some(t);
        self.bus.cart.set_rtc(t, self.bus.sched.now());
    }

    /// The options the machine was built with.
    pub fn options(&self) -> CartOptions {
        self.options
    }

    /// FNV-1a of the real BIOS dump the machine boots through, `None` for the
    /// built-in HLE BIOS.
    pub fn bios_hash(&self) -> Option<u64> {
        matches!(self.bus.bios, Bios::Real(_)).then_some(self.bios_hash)
    }

    /// The start time given to [`Gba::set_rtc`], if any.
    pub fn rtc_start(&self) -> Option<RtcTime> {
        self.rtc_start
    }

    /// Snapshot the whole machine (see [`crate::state`] for the format).
    ///
    /// Host configuration and buffers are not included: the audio sample rate,
    /// undrained audio samples and the battery-save generation counter.
    pub fn save_state(&self) -> Vec<u8> {
        let mut w = StateWriter::with_capacity(MAX_STATE_BYTES);
        w.bytes(&state::MAGIC);
        w.u16(state::VERSION);
        w.u64(self.rom_hash);
        w.u8(self.bios_kind());
        w.u64(self.bios_hash);
        w.u8(self.options.save.id());
        w.u8(self.options.rtc.id());
        w.u8(tag::CPU);
        self.cpu.save_state(&mut w);
        self.bus.save_state(&mut w);
        w.u8(tag::RTC_START);
        save_rtc_start(&mut w, self.rtc_start);
        w.into_bytes()
    }

    /// 0 for the built-in BIOS, 1 for a real dump.
    fn bios_kind(&self) -> u8 {
        u8::from(matches!(self.bus.bios, Bios::Real(_)))
    }

    /// Restore a snapshot from [`Gba::save_state`] taken with the same ROM, BIOS
    /// and cartridge options.
    ///
    /// All-or-nothing: on error the machine is left exactly as it was. On
    /// success buffered audio is discarded (it belongs to the old timeline), the
    /// keypad holds whatever buttons were held when the state was saved (the host
    /// should re-apply its current input), and [`Gba::battery_generation`]
    /// advances so the host writes the restored save data.
    ///
    /// A different ROM is [`StateError::RomMismatch`]; a different BIOS kind or
    /// dump, save type or clock option is [`StateError::ModelMismatch`].
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), StateError> {
        let mut r = StateReader::new(data);
        let mut magic = [0; 4];
        r.bytes_into(&mut magic).map_err(|_| StateError::BadMagic)?;
        if magic != state::MAGIC {
            return Err(StateError::BadMagic);
        }
        let version = r.u16()?;
        if version != state::VERSION {
            return Err(StateError::UnsupportedVersion(version));
        }
        if r.u64()? != self.rom_hash {
            return Err(StateError::RomMismatch);
        }
        let kind = r.u8()?;
        let bios_hash = r.u64()?;
        check(kind <= 1, "BIOS kind")?;
        check(
            kind == 1 || bios_hash == 0,
            "BIOS hash of the built-in BIOS",
        )?;
        let save = SaveChoice::from_id(r.u8()?).ok_or(StateError::Invalid("save type"))?;
        let rtc = RtcMode::from_id(r.u8()?).ok_or(StateError::Invalid("clock option"))?;
        if kind != self.bios_kind()
            || bios_hash != self.bios_hash
            || save != self.options.save
            || rtc != self.options.rtc
        {
            return Err(StateError::ModelMismatch);
        }
        // Load into a scratch machine and swap it in only on success, so a state
        // that fails anywhere leaves this one untouched and nothing depends on
        // this machine's own state passing the validators (an earlier rollback re-loaded a snapshot of it and could panic the host).
        let mut next = self.scratch();
        next.load_sections(r)?;
        next.bus.cart.exchange_rom(&mut self.bus.cart);
        next.bus.cart.bump_generation();
        *self = next;
        Ok(())
    }

    /// A powered-off machine of the same ROM, BIOS and options with the host's
    /// audio rate and the cartridge's save generation, to load a state into. The
    /// ROM stays with `self` until the load succeeded ([`Cart::exchange_rom`]).
    fn scratch(&self) -> Self {
        let mut bus = Bus::new(self.bus.cart.blank_like(), self.bus.bios.clone());
        bus.apu.set_sample_rate(self.bus.apu.sample_rate());
        Self {
            cpu: Cpu::reset(),
            bus,
            rom_hash: self.rom_hash,
            options: self.options,
            bios_hash: self.bios_hash,
            rtc_start: None,
        }
    }

    fn load_sections(&mut self, mut r: StateReader) -> Result<(), StateError> {
        r.tag(tag::CPU)?;
        self.cpu.load_state(&mut r)?;
        self.bus.load_state(&mut r)?;
        r.tag(tag::RTC_START)?;
        self.rtc_start = load_rtc_start(&mut r)?;
        r.finish()
    }

    /// The cartridge clock's date and time now, in emulated time.
    pub fn rtc_time(&self) -> RtcTime {
        self.bus.cart.rtc_time(self.bus.sched.now())
    }

    /// Master clock in CPU cycles since power-on.
    pub fn cycles(&self) -> u64 {
        self.bus.sched.now()
    }

    /// Frames completed since power-on.
    pub fn frames(&self) -> u64 {
        self.bus.frames
    }
}

#[cfg(test)]
mod state_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_frame_runs_one_frame_of_cycles() {
        let mut gba = Gba::new(vec![0; 0x200], None).unwrap();
        gba.step_frame();
        assert_eq!(gba.frames(), 1);
        // The first VBlank starts at line 160.
        assert_eq!(
            gba.cycles(),
            u64::from(crate::CYCLES_PER_LINE * crate::VISIBLE_LINES)
        );
        gba.step_frame();
        assert_eq!(gba.frames(), 2);
        assert_eq!(
            gba.cycles(),
            u64::from(crate::CYCLES_PER_LINE * crate::VISIBLE_LINES + CYCLES_PER_FRAME)
        );
    }

    #[test]
    fn step_instruction_skips_a_halt_and_runs_dma_a_unit_at_a_time() {
        let mut gba = Gba::new(vec![0; 0x200], None).unwrap();
        // HALTCNT (high byte of 0x300): every step now advances to an event.
        gba.bus.write_io16(0x300, 0, 0xFF00);
        assert!(gba.bus.halted);
        // The power-on line-start event is due at cycle 0: the first step only
        // dispatches it.
        gba.step_instruction();
        let t = gba.cycles();
        gba.step_instruction();
        assert!(gba.cycles() > t, "halt skipped the clock");
        // An immediate DMA3 of 3 halfwords, IWRAM to IWRAM, runs even halted.
        let io = |gba: &mut Gba, off, v: u32| {
            gba.bus.write_io16(off, v as u16, 0xFFFF);
            gba.bus.write_io16(off + 2, (v >> 16) as u16, 0xFFFF);
        };
        io(&mut gba, 0xD4, 0x0300_0000);
        io(&mut gba, 0xD8, 0x0300_0100);
        io(&mut gba, 0xDC, 3 | 0x8000 << 16);
        let t = gba.cycles();
        gba.step_instruction(); // start delay + first unit
        assert_eq!(gba.cycles() - t, 2 + 2);
        gba.step_instruction();
        assert_eq!(gba.cycles() - t, 2 + 4);
        gba.step_instruction();
        assert!(gba.bus.dma.active_channel().is_none());
    }

    /// `step_once(false)` without the dispatch gate: events are processed after
    /// every step, as the run loop did before P3-TIMING.
    fn step_ungated(g: &mut Gba) {
        if g.bus.dma.active_channel().is_some() {
            g.bus.run_dma();
        } else if g.bus.halted {
            if !g.bus.sched.skip_to_next() {
                return;
            }
        } else {
            g.cpu.step(&mut g.bus);
        }
        g.bus.process_events();
    }

    /// A ROM that starts timer 0 (reload 0xFFF0, 16 cycles per overflow) and then
    /// reads its counter in a loop, so timer, PPU and APU events are all crossed.
    fn timer_rom() -> Vec<u8> {
        let mut rom = vec![0; 0x100];
        let words: [(usize, u32); 7] = [
            (0x00, 0xEA00_002E), // b 0xC0
            (0xC0, 0xE3A0_0301), // mov r0, #0x04000000
            (0xC4, 0xE3A0_1880), // mov r1, #0x00800000 (enable)
            (0xC8, 0xE381_1CF0), // orr r1, r1, #0xF000
            (0xCC, 0xE381_10F0), // orr r1, r1, #0xF0 (reload 0xFFF0)
            (0xD0, 0xE580_1100), // str r1, [r0, #0x100]
            (0xD4, 0xE590_2100), // loop: ldr r2, [r0, #0x100]
        ];
        for (at, w) in words {
            rom[at..at + 4].copy_from_slice(&w.to_le_bytes());
        }
        rom[0xD8..0xDC].copy_from_slice(&0xEAFF_FFFDu32.to_le_bytes()); // b loop
        rom
    }

    #[test]
    fn gating_process_events_does_not_change_results() {
        let mut gated = Gba::new(timer_rom(), None).unwrap();
        let mut plain = Gba::new(timer_rom(), None).unwrap();
        for i in 0..300_000 {
            gated.step_once(false);
            step_ungated(&mut plain);
            if i % 1000 == 0 {
                assert_eq!(gated.cycles(), plain.cycles());
                assert_eq!(gated.cpu_registers(), plain.cpu_registers());
            }
        }
        assert_eq!(gated.cycles(), plain.cycles());
        assert_eq!(gated.frames(), plain.frames());
        assert!(gated.frames() >= 1, "crossed frame events");
        assert_eq!(gated.cpu_registers(), plain.cpu_registers());
        assert_eq!(gated.peek16(0x0400_0006), plain.peek16(0x0400_0006));
        assert_eq!(gated.peek16(0x0400_0202), plain.peek16(0x0400_0202));
    }

    /// A ROM whose code at 0xC0 is `words`, entered through a branch at 0.
    fn rom_with(words: &[u32]) -> Vec<u8> {
        let mut rom = vec![0; 0x100 + 4 * words.len()];
        rom[0..4].copy_from_slice(&0xEA00_002Eu32.to_le_bytes()); // b 0xC0
        for (i, w) in words.iter().enumerate() {
            rom[0xC0 + 4 * i..0xC4 + 4 * i].copy_from_slice(&w.to_le_bytes());
        }
        rom
    }

    const MODE_IRQ: u32 = 0x12;

    /// Run `steps` steps on a gated and an ungated machine in lockstep, asserting
    /// they never differ in cycles or registers, and call `each` on the gated one
    /// after every step.
    fn lockstep(
        rom: Vec<u8>,
        steps: usize,
        mut setup: impl FnMut(&mut Gba),
        mut each: impl FnMut(usize, &Gba),
    ) {
        let mut gated = Gba::new(rom.clone(), None).unwrap();
        let mut plain = Gba::new(rom, None).unwrap();
        setup(&mut gated);
        setup(&mut plain);
        for i in 0..steps {
            gated.step_once(false);
            step_ungated(&mut plain);
            assert_eq!(gated.cycles(), plain.cycles(), "step {i}");
            assert_eq!(gated.cpu_registers(), plain.cpu_registers(), "step {i}");
            assert_eq!(gated.bus.halted, plain.bus.halted, "step {i}");
            assert_eq!(gated.bus.irq.if_, plain.bus.irq.if_, "step {i}");
            each(i, &gated);
        }
    }

    #[test]
    fn an_ie_ime_write_that_makes_an_irq_pending_is_taken_before_the_next_instruction() {
        // The IRQ line is level (IME & IE & IF), not an event, so the
        // gate cannot delay it. VBlank is already flagged; IE then IME open the gate.
        let rom = rom_with(&[
            0xE3A0_0301, // mov r0, #0x04000000
            0xE3A0_1001, // mov r1, #1
            0xE580_1200, // str r1, [r0, #0x200]   IE = VBlank
            0xE580_1208, // str r1, [r0, #0x208]   IME = 1
            0xE1A0_0000, // nop
            0xEAFF_FFFE, // b .
        ]);
        let mut ime_step = None;
        let mut irq_step = None;
        lockstep(
            rom,
            12,
            |g| g.bus.irq.if_ = 1,
            |i, g| {
                if ime_step.is_none() && g.bus.irq.ime {
                    ime_step = Some(i);
                }
                if irq_step.is_none() && g.cpu_registers().cpsr & 0x1F == MODE_IRQ {
                    irq_step = Some(i);
                }
            },
        );
        let ime = ime_step.expect("IME was written");
        assert_eq!(irq_step, Some(ime + 1), "taken at the very next step");
    }

    #[test]
    fn haltcnt_with_a_pending_interrupt_never_halts() {
        let rom = rom_with(&[
            0xE3A0_0301, // mov r0, #0x04000000
            0xE3A0_1001, // mov r1, #1
            0xE580_1200, // str r1, [r0, #0x200]   IE = VBlank (IME stays 0)
            0xE3A0_2000, // mov r2, #0
            0xE5C0_2301, // strb r2, [r0, #0x301]  HALTCNT
            0xE3A0_3007, // mov r3, #7
            0xEAFF_FFFE, // b .
        ]);
        let mut done = false;
        lockstep(
            rom,
            16,
            |g| g.bus.irq.if_ = 1,
            |_, g| {
                assert!(!g.bus.halted, "halted with an enabled interrupt pending");
                done |= g.cpu_registers().r[3] == 7;
            },
        );
        assert!(done, "ran on past the HALTCNT write");
    }

    #[test]
    fn a_halt_woken_by_a_timer_irq_resumes_at_the_same_cycle_as_without_gating() {
        let rom = rom_with(&[
            0xE3A0_0301, // mov r0, #0x04000000
            0xE3A0_18C0, // mov r1, #0x00C00000   enable + IRQ
            0xE381_1CF0, // orr r1, r1, #0xF000
            0xE381_10F0, // orr r1, r1, #0xF0     reload 0xFFF0: overflow after 16 cycles
            0xE580_1100, // str r1, [r0, #0x100]  TM0CNT
            0xE3A0_2008, // mov r2, #8
            0xE580_2200, // str r2, [r0, #0x200]  IE = Timer0 (IME stays 0)
            0xE3A0_2000, // mov r2, #0
            0xE5C0_2301, // strb r2, [r0, #0x301] HALTCNT
            0xE3A0_3007, // mov r3, #7
            0xEAFF_FFFE, // b .
        ]);
        let (mut halted_at, mut woke_at) = (None, None);
        lockstep(
            rom,
            24,
            |_| {},
            |_, g| {
                if g.bus.halted && halted_at.is_none() {
                    halted_at = Some(g.cycles());
                }
                if halted_at.is_some() && !g.bus.halted && woke_at.is_none() {
                    woke_at = Some(g.cycles());
                }
            },
        );
        let (h, w) = (halted_at.expect("halted"), woke_at.expect("woken"));
        assert!(w > h, "the clock skipped to the timer overflow");
    }
}
