//! The public emulator facade.

use crate::bus::Bus;
use crate::cartridge::{self, CartridgeError, CartridgeHeader};
use crate::cpu::{Cpu, Registers};
use crate::joypad::Button;
use crate::model::{Hw, Model};
use crate::state::{self, StateError, StateReader, StateWriter, check, tag};
use crate::{CgbFramebuffer, Framebuffer, M_CYCLES_PER_FRAME};

/// Magic + version + ROM hash + model + hardware mode.
const STATE_HEADER_LEN: usize = 4 + 2 + 8 + 1 + 1;

/// The first state-format version, before there was a model: always a DMG, with
/// no model or hardware-mode byte (the sections that follow are identical).
const STATE_VERSION_DMG_ONLY: u16 = 1;

/// A complete Game Boy (DMG or CGB): CPU + bus (which owns every other component).
pub struct GameBoy {
    cpu: Cpu,
    bus: Bus,
    /// FNV-1a of the ROM image; ties save states to the ROM they came from.
    rom_hash: u64,
    model: Model,
}

impl GameBoy {
    /// Power on a DMG with `rom` inserted, in the post-boot-ROM state.
    ///
    /// Always a DMG, whatever the header says; see [`GameBoy::with_model`].
    pub fn new(rom: Vec<u8>) -> Result<Self, CartridgeError> {
        Self::with_model(rom, Model::Dmg)
    }

    /// Power on `model` with `rom` inserted, in the post-boot-ROM state.
    ///
    /// The core never picks the model from the ROM (DECISIONS #31); a host that
    /// wants that can call [`Model::from_header`] on a
    /// [`CartridgeHeader::parse`] of the ROM first.
    pub fn with_model(rom: Vec<u8>, model: Model) -> Result<Self, CartridgeError> {
        let rom_hash = state::fnv1a(&rom);
        let cart = cartridge::from_rom(rom)?;
        let bus = Bus::with_model(cart, model);
        Ok(Self {
            cpu: Cpu::with_registers(bus.post_boot_registers()),
            bus,
            rom_hash,
            model,
        })
    }

    /// The model this machine was powered on as.
    pub fn model(&self) -> Model {
        self.model
    }

    /// Replace the cartridge and power-cycle the machine (same model).
    ///
    /// Host configuration survives (the audio sample rate set by
    /// [`GameBoy::set_audio_sample_rate`]); all emulated state is reset,
    /// including held buttons, which the host must re-press. On error the
    /// machine is left untouched.
    pub fn load_rom(&mut self, rom: Vec<u8>) -> Result<(), CartridgeError> {
        let mut fresh = Self::with_model(rom, self.model)?;
        fresh.set_audio_sample_rate(self.audio_sample_rate());
        *self = fresh;
        Ok(())
    }

    pub fn cartridge_header(&self) -> &CartridgeHeader {
        self.bus.cart.header()
    }

    /// Run until the PPU completes a frame (≈70224 T-cycles).
    ///
    /// A safety cap of two frames' worth of cycles guarantees termination even
    /// if a component misbehaves.
    pub fn step_frame(&mut self) {
        let limit = self.bus.mcycles() + 2 * M_CYCLES_PER_FRAME as u64;
        while !self.bus.ppu.take_frame_ready() && self.bus.mcycles() < limit {
            self.cpu.step(&mut self.bus);
        }
    }

    /// Execute one CPU step (instruction, interrupt dispatch or halted cycle).
    pub fn step_instruction(&mut self) {
        self.cpu.step(&mut self.bus);
    }

    /// Last completed frame: 160x144 shades 0..=3 (0 = lightest). On a CGB
    /// running in colour mode ([`GameBoy::cgb_framebuffer`] is `Some`) the frame is
    /// not shades: this buffer is then all zero, never written (DECISIONS #36).
    pub fn framebuffer(&self) -> &Framebuffer {
        self.bus.ppu.framebuffer()
    }

    /// Last completed frame in RGB555 (`0bbbbbgggggrrrrr`, bit 15 clear), `Some`
    /// iff the machine renders in CGB colour mode: a [`Model::Cgb`] running a
    /// CGB-flagged cartridge. A DMG, and a CGB in DMG-compatibility mode, return
    /// `None` and draw shades into [`GameBoy::framebuffer`]. The LCD-off frame is
    /// white (0x7FFF).
    pub fn cgb_framebuffer(&self) -> Option<&CgbFramebuffer> {
        self.bus.ppu.cgb_framebuffer()
    }

    pub fn set_button(&mut self, button: Button, pressed: bool) {
        self.bus
            .joypad
            .set_button(button, pressed, &mut self.bus.interrupts);
    }

    /// Drain buffered audio: interleaved stereo `f32` in -1.0..=1.0 at
    /// [`GameBoy::audio_sample_rate`].
    ///
    /// Allocates a new `Vec` per call; per-frame callers should prefer
    /// [`GameBoy::drain_audio_into`].
    pub fn audio_samples(&mut self) -> Vec<f32> {
        let mut out = Vec::new();
        self.drain_audio_into(&mut out);
        out
    }

    /// Like [`GameBoy::audio_samples`] but appends to `out` (existing contents
    /// are kept), so a caller can reuse one buffer across frames.
    pub fn drain_audio_into(&mut self, out: &mut Vec<f32>) {
        self.bus.apu.drain_samples(out);
    }

    pub fn set_audio_sample_rate(&mut self, rate: u32) {
        self.bus.apu.set_sample_rate(rate);
    }

    pub fn audio_sample_rate(&self) -> u32 {
        self.bus.apu.sample_rate()
    }

    /// Battery-backed RAM to write to a `.sav` file (`None` without a battery).
    pub fn battery_ram(&self) -> Option<Vec<u8>> {
        self.bus.cart.battery_ram()
    }

    /// Counter that advances only when the game changes battery-backed state
    /// (a cart-RAM byte, an RTC register, a latch that changes the latched
    /// registers) or a save is loaded; never on RTC ticks. Cheap to poll, so a
    /// host can skip cloning `battery_ram()` and rewriting the `.sav` while it
    /// is unchanged, and still write one final image on quit to capture the
    /// free-running RTC. Only meaningful when `battery_ram()` is `Some`.
    pub fn battery_generation(&self) -> u64 {
        self.bus.cart.battery_generation()
    }

    /// Restore battery-backed RAM from a `.sav` file.
    pub fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        self.bus.cart.load_battery_ram(data)
    }

    /// Snapshot the whole machine (see [`crate::state`] for the format).
    ///
    /// Host configuration and buffers are not included: the audio sample rate,
    /// undrained audio samples and captured serial output.
    pub fn save_state(&self) -> Vec<u8> {
        let mut w = StateWriter::new();
        w.bytes(&state::MAGIC);
        w.u16(state::VERSION);
        w.u64(self.rom_hash);
        w.u8(match self.model {
            Model::Dmg => 0,
            Model::Cgb => 1,
        });
        w.u8(self.bus.hw.id());
        w.u8(tag::CPU);
        self.cpu.save_state(&mut w);
        self.bus.save_state(&mut w);
        w.into_bytes()
    }

    /// Restore a snapshot from [`GameBoy::save_state`] taken with the same ROM
    /// and model.
    ///
    /// All-or-nothing: on error the machine is left exactly as it was. On
    /// success buffered audio is discarded (it belongs to the old timeline),
    /// the joypad holds whatever buttons were held when the state was saved
    /// (the host should re-apply its current input), and
    /// [`GameBoy::battery_generation`] advances so the host writes the
    /// restored battery RAM.
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), StateError> {
        let mut r = StateReader::new(data);
        let mut magic = [0; 4];
        r.bytes_into(&mut magic).map_err(|_| StateError::BadMagic)?;
        if magic != state::MAGIC {
            return Err(StateError::BadMagic);
        }
        let version = r.u16()?;
        if version != state::VERSION && version != STATE_VERSION_DMG_ONLY {
            return Err(StateError::UnsupportedVersion(version));
        }
        if r.u64()? != self.rom_hash {
            return Err(StateError::RomMismatch);
        }
        let model = if version == STATE_VERSION_DMG_ONLY {
            Model::Dmg
        } else {
            match r.u8()? {
                0 => Model::Dmg,
                1 => Model::Cgb,
                _ => return Err(StateError::Invalid("model")),
            }
        };
        if model != self.model {
            return Err(StateError::ModelMismatch);
        }
        if version != STATE_VERSION_DMG_ONLY {
            // A function of the model and the ROM, so a mismatch with a matching
            // hash and model means the byte is corrupt.
            let hw = r.u8()?;
            check(hw <= Hw::MAX_ID, "hardware mode")?;
            check(hw == self.bus.hw.id(), "hardware mode")?;
        }
        let backup = self.save_state();
        if let Err(e) = self.load_sections(r) {
            // Our own fresh output always decodes.
            self.load_sections(StateReader::new(&backup[STATE_HEADER_LEN..]))
                .expect("restoring the pre-load snapshot failed");
            return Err(e);
        }
        self.bus.apu.drain_samples(&mut Vec::new());
        Ok(())
    }

    fn load_sections(&mut self, mut r: StateReader) -> Result<(), StateError> {
        r.tag(tag::CPU)?;
        self.cpu.load_state(&mut r)?;
        self.bus.load_state(&mut r)?;
        r.finish()
    }

    // ---- Test / debug hooks (pure, no I/O) ----

    /// Bytes written to the serial port so far.
    pub fn serial_output(&self) -> &[u8] {
        self.bus.serial.output()
    }

    pub fn take_serial_output(&mut self) -> Vec<u8> {
        self.bus.serial.take_output()
    }

    pub fn cpu_registers(&self) -> Registers {
        *self.cpu.registers()
    }

    /// True (once) if the CPU executed `LD B,B` since the last call.
    pub fn take_ld_b_b_breakpoint(&mut self) -> bool {
        self.cpu.take_ld_b_b_breakpoint()
    }

    /// True if the CPU hit an illegal opcode and hung.
    pub fn cpu_locked(&self) -> bool {
        self.cpu.is_locked()
    }

    /// M-cycles since power-on.
    pub fn mcycles(&self) -> u64 {
        self.bus.mcycles()
    }

    /// Read the memory map as the PPU and cartridge mapper currently present it,
    /// without side effects (`&self`: no clocks advance, nothing is latched or
    /// acknowledged). It is not a raw dump of the underlying memories:
    ///
    /// * With the LCD on, VRAM (0x8000-0x9FFF) reads 0xFF while the PPU is in
    ///   mode 3 and OAM (0xFE00-0xFE9F) reads 0xFF in modes 2 and 3, as for the CPU.
    /// * Cartridge RAM (0xA000-0xBFFF) goes through the mapper: 0xFF while the
    ///   game has RAM disabled (MBC1/3/5), the selected bank, or the latched
    ///   RTC register on MBC3 when one is mapped.
    /// * OAM DMA bus conflicts (which the CPU would see) are not applied: a running
    ///   DMA does not change what `peek` returns.
    pub fn peek(&self, addr: u16) -> u8 {
        self.bus.peek(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::CpuBus;

    fn nop_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x147] = 0x00;
        rom
    }

    #[test]
    fn step_frame_takes_one_frame_of_cycles() {
        let mut gb = GameBoy::new(nop_rom()).unwrap();
        gb.step_frame(); // partial first frame (boot state starts mid-frame)
        let start = gb.mcycles();
        gb.step_frame();
        assert_eq!(gb.mcycles() - start, M_CYCLES_PER_FRAME as u64);
    }

    #[test]
    fn load_rom_preserves_audio_sample_rate() {
        let mut gb = GameBoy::new(nop_rom()).unwrap();
        gb.set_audio_sample_rate(44_100);
        gb.load_rom(nop_rom()).unwrap();
        assert_eq!(gb.audio_sample_rate(), 44_100);
        // A rejected ROM leaves the machine (and its rate) alone.
        assert!(gb.load_rom(vec![0; 4]).is_err());
        assert_eq!(gb.audio_sample_rate(), 44_100);
        // And the rate actually drives sample generation after the reload.
        for _ in 0..60 {
            gb.step_frame();
        }
        let n = gb.audio_samples().len() / 2;
        assert!((44_000..44_400).contains(&n), "{n}");
    }

    #[test]
    fn drain_audio_into_appends_and_matches_audio_samples() {
        let mut a = GameBoy::new(nop_rom()).unwrap();
        let mut b = GameBoy::new(nop_rom()).unwrap();
        a.step_frame();
        b.step_frame();
        let mut buf = vec![7.0];
        a.drain_audio_into(&mut buf);
        assert_eq!(buf[0], 7.0, "existing contents kept");
        assert_eq!(&buf[1..], b.audio_samples().as_slice());
        let mut again = Vec::new();
        a.drain_audio_into(&mut again);
        assert!(again.is_empty(), "drained");
    }

    /// Run `n` frames and return (frame hash, audio samples as bits).
    fn run(gb: &mut GameBoy, n: u32) -> (u64, Vec<u32>) {
        let mut h = 0u64;
        for _ in 0..n {
            gb.step_frame();
            h = state::fnv1a(gb.framebuffer()) ^ h.rotate_left(1);
        }
        let audio = gb.audio_samples().iter().map(|s| s.to_bits()).collect();
        (h, audio)
    }

    #[test]
    fn save_state_round_trip_is_deterministic() {
        let mut a = GameBoy::new(nop_rom()).unwrap();
        run(&mut a, 30);
        let snap = a.save_state();
        let expected = run(&mut a, 30);

        let mut b = GameBoy::new(nop_rom()).unwrap();
        run(&mut b, 7);
        b.load_state(&snap).unwrap();
        assert_eq!(b.save_state(), snap, "load then save is byte-identical");
        assert_eq!(run(&mut b, 30), expected);
        assert_eq!(a.save_state(), b.save_state());
    }

    #[test]
    fn load_state_discards_buffered_audio() {
        let mut gb = GameBoy::new(nop_rom()).unwrap();
        let snap = gb.save_state();
        gb.step_frame();
        gb.load_state(&snap).unwrap();
        assert!(gb.audio_samples().is_empty());
    }

    #[test]
    fn host_config_survives_load_state() {
        let mut gb = GameBoy::new(nop_rom()).unwrap();
        let snap = gb.save_state();
        gb.set_audio_sample_rate(44_100);
        gb.load_state(&snap).unwrap();
        assert_eq!(gb.audio_sample_rate(), 44_100);
    }

    #[test]
    fn rejects_header_problems() {
        let mut gb = GameBoy::new(nop_rom()).unwrap();
        gb.step_frame();
        let snap = gb.save_state();
        let mut bad = snap.clone();
        bad[0] = b'X';
        assert_eq!(gb.load_state(&bad), Err(StateError::BadMagic));
        assert_eq!(gb.load_state(b"GB"), Err(StateError::BadMagic));
        let mut bad = snap.clone();
        bad[4] = 99;
        assert_eq!(gb.load_state(&bad), Err(StateError::UnsupportedVersion(99)));
        let mut other_rom = nop_rom();
        other_rom[0x200] = 1;
        let other = GameBoy::new(other_rom).unwrap().save_state();
        assert_eq!(gb.load_state(&other), Err(StateError::RomMismatch));
        let mut long = snap.clone();
        long.push(0);
        assert_eq!(gb.load_state(&long), Err(StateError::TrailingBytes));
        assert_eq!(gb.save_state(), snap, "nothing changed");
    }

    #[test]
    fn truncated_state_is_rejected_and_changes_nothing() {
        let mut src = GameBoy::new(nop_rom()).unwrap();
        run(&mut src, 3);
        let snap = src.save_state();
        let mut gb = GameBoy::new(nop_rom()).unwrap();
        let before = gb.save_state();
        // Every cut inside the header, then a stride through the body (each
        // failed load snapshots and restores the machine, so all 70K is slow).
        let cuts =
            (0..STATE_HEADER_LEN + 64).chain((STATE_HEADER_LEN + 64..snap.len()).step_by(61));
        for len in cuts {
            assert!(gb.load_state(&snap[..len]).is_err(), "len {len}");
        }
        assert_eq!(gb.save_state(), before);
    }

    #[test]
    fn corrupt_fields_never_panic() {
        let mut src = GameBoy::new(nop_rom()).unwrap();
        run(&mut src, 5);
        let snap = src.save_state();
        let mut gb = GameBoy::new(nop_rom()).unwrap();
        for pos in (STATE_HEADER_LEN..snap.len()).step_by(7) {
            for v in [0xFF, 0x80, 0x00] {
                let mut bad = snap.clone();
                bad[pos] = v;
                let before = gb.save_state();
                match gb.load_state(&bad) {
                    // Whatever got through validation must still run.
                    Ok(()) => gb.step_frame(),
                    Err(_) => assert_eq!(gb.save_state(), before, "pos {pos}"),
                }
            }
        }
    }

    fn cgb_rom() -> Vec<u8> {
        let mut rom = nop_rom();
        rom[0x143] = 0x80;
        rom
    }

    #[test]
    fn new_is_always_a_dmg_and_with_model_is_explicit() {
        assert_eq!(GameBoy::new(cgb_rom()).unwrap().model(), Model::Dmg);
        let gb = GameBoy::with_model(cgb_rom(), Model::Dmg).unwrap();
        assert_eq!(gb.model(), Model::Dmg);
        assert_eq!(gb.cpu_registers().a, 0x01);
        let mut gb = GameBoy::with_model(nop_rom(), Model::Cgb).unwrap();
        assert_eq!(gb.model(), Model::Cgb);
        assert_eq!(gb.cpu_registers().a, 0x11);
        // A reload keeps the model.
        gb.load_rom(cgb_rom()).unwrap();
        assert_eq!(gb.model(), Model::Cgb);
        assert_eq!(gb.cpu_registers().d, 0xFF);
        // The hash is the same one `new` computes: a DMG state of the same ROM
        // is refused for its model, not for its ROM.
        let dmg = GameBoy::new(cgb_rom()).unwrap().save_state();
        assert_eq!(gb.load_state(&dmg), Err(StateError::ModelMismatch));
    }

    #[test]
    fn cgb_save_state_round_trips_banks_and_registers() {
        let mut a = GameBoy::with_model(cgb_rom(), Model::Cgb).unwrap();
        run(&mut a, 3);
        for (addr, v) in [
            (0xFF40, 0x00), // LCD off so VRAM is writable
            (0xFF4F, 1),
            (0x8010, 0xB1),
            (0xFF4F, 0),
            (0x8010, 0xA0),
            (0xFF70, 5),
            (0xD123, 0x55),
            (0xFF70, 2),
            (0xD123, 0x22),
            (0xFF4D, 1),
            (0xFF72, 0x99),
            (0xFF74, 0x6B),
            (0xFF75, 0x30),
            (0xFF40, 0x91),
        ] {
            a.bus.write(addr, v);
        }
        a.audio_samples(); // host buffers are not part of a state
        let snap = a.save_state();
        let expected = run(&mut a, 10);

        let mut b = GameBoy::with_model(cgb_rom(), Model::Cgb).unwrap();
        run(&mut b, 5);
        b.load_state(&snap).unwrap();
        assert_eq!(b.save_state(), snap, "load then save is byte-identical");
        assert_eq!(b.bus.ppu.vram_bank(1)[0x10], 0xB1);
        assert_eq!(b.bus.ppu.vram_bank(0)[0x10], 0xA0);
        assert_eq!(b.peek(0xFF70), 0xFA);
        assert_eq!(b.peek(0xD123), 0x22);
        assert_eq!(b.peek(0xFF4D), 0x7F);
        assert_eq!(b.peek(0xFF72), 0x99);
        assert_eq!(b.peek(0xFF74), 0x6B);
        assert_eq!(b.peek(0xFF75), 0xBF);
        assert_eq!(run(&mut b, 10), expected);
        assert_eq!(a.save_state(), b.save_state());
        // The other WRAM bank came across too.
        b.bus.write(0xFF70, 5);
        assert_eq!(b.bus.read(0xD123), 0x55);
    }

    #[test]
    fn states_do_not_cross_models() {
        let mut dmg = GameBoy::new(nop_rom()).unwrap();
        let mut cgb = GameBoy::with_model(nop_rom(), Model::Cgb).unwrap();
        let (dmg_state, cgb_state) = (dmg.save_state(), cgb.save_state());
        assert_ne!(dmg_state.len(), cgb_state.len());
        assert_eq!(dmg.load_state(&cgb_state), Err(StateError::ModelMismatch));
        assert_eq!(cgb.load_state(&dmg_state), Err(StateError::ModelMismatch));
        assert_eq!(dmg.save_state(), dmg_state, "nothing changed");
        assert_eq!(cgb.save_state(), cgb_state, "nothing changed");
        // An unknown model byte is corruption, not a mismatch.
        let mut bad = cgb_state.clone();
        bad[STATE_HEADER_LEN - 2] = 7;
        assert_eq!(cgb.load_state(&bad), Err(StateError::Invalid("model")));
    }

    /// The index of the single byte that differs between two states.
    fn only_diff(a: &[u8], b: &[u8]) -> usize {
        assert_eq!(a.len(), b.len());
        let d: Vec<_> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
        assert_eq!(d.len(), 1, "{d:?}");
        d[0]
    }

    #[test]
    fn corrupt_cgb_bank_selectors_are_rejected() {
        // Two machines that differ only in the selector (same cycle counts).
        let with = |addr, v| {
            let mut gb = GameBoy::with_model(cgb_rom(), Model::Cgb).unwrap();
            gb.bus.write(addr, v);
            gb
        };
        let svbk1 = with(0xFF70, 1).save_state();
        let svbk6 = with(0xFF70, 6).save_state();
        let svbk_at = only_diff(&svbk1, &svbk6);
        let mut bad = svbk6.clone();
        bad[svbk_at] = 8;
        let mut gb = with(0xFF70, 3);
        let before = gb.save_state();
        assert_eq!(gb.load_state(&bad), Err(StateError::Invalid("SVBK")));
        assert_eq!(gb.save_state(), before, "rolled back");

        let mut a = with(0xFF4F, 0);
        let vbk0 = a.save_state();
        let vbk1 = with(0xFF4F, 1).save_state();
        let vbk_at = only_diff(&vbk0, &vbk1);
        let mut bad = vbk1.clone();
        bad[vbk_at] = 2;
        assert_eq!(a.load_state(&bad), Err(StateError::Invalid("VBK")));
        assert_eq!(a.save_state(), vbk0);
    }

    #[test]
    fn corrupt_cgb_fields_never_panic() {
        let mut src = GameBoy::with_model(cgb_rom(), Model::Cgb).unwrap();
        run(&mut src, 5);
        let snap = src.save_state();
        let mut gb = GameBoy::with_model(cgb_rom(), Model::Cgb).unwrap();
        for pos in (STATE_HEADER_LEN..snap.len()).step_by(11) {
            for v in [0xFF, 0x80, 0x02] {
                let mut bad = snap.clone();
                bad[pos] = v;
                let before = gb.save_state();
                match gb.load_state(&bad) {
                    Ok(()) => gb.step_frame(),
                    Err(_) => assert_eq!(gb.save_state(), before, "pos {pos}"),
                }
            }
        }
    }

    /// The ROM `tests/fixtures/state_v1_dmg.bin` was taken with: fills WRAM,
    /// starts CH1, then counts in HRAM forever.
    fn v1_fixture_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x100..0x103].copy_from_slice(&[0xC3, 0x50, 0x01]);
        let prog: &[u8] = &[
            0x21, 0x00, 0xC0, 0x06, 0x40, 0x78, 0x22, 0x05, 0x20, 0xFB, 0x3E, 0x77, 0xE0, 0x24,
            0x3E, 0xF0, 0xE0, 0x12, 0x3E, 0x87, 0xE0, 0x14, 0x3C, 0xE0, 0x80, 0x18, 0xFB,
        ];
        rom[0x150..0x150 + prog.len()].copy_from_slice(prog);
        rom
    }

    /// A real version-1 state: written by the `feat/save-states` writer
    /// (`ae936d7`) after `GameBoy::new(v1_fixture_rom())` ran 3 frames.
    const STATE_V1: &[u8] = include_bytes!("../tests/fixtures/state_v1_dmg.bin");
    const V1_HEADER_LEN: usize = 4 + 2 + 8;

    #[test]
    fn version_1_states_load_as_a_dmg() {
        assert_eq!(&STATE_V1[4..6], &[1, 0], "the fixture is a version-1 state");
        let mut gb = GameBoy::new(v1_fixture_rom()).unwrap();
        gb.load_state(STATE_V1).unwrap();
        assert_eq!(gb.model(), Model::Dmg);
        let r = gb.cpu_registers();
        assert_eq!((r.a, r.pc, r.sp), (0x09, 0x0169, 0xFFFE));
        assert_eq!(gb.mcycles(), 51554);
        assert_eq!((gb.peek(0xC000), gb.peek(0xC03F)), (0x40, 0x01));
        // It carries on exactly as the machine that wrote it did.
        gb.step_frame();
        gb.step_frame();
        let r = gb.cpu_registers();
        assert_eq!((gb.mcycles(), r.a, gb.peek(0xFF80)), (86666, 0xA1, 0xA1));
        assert_eq!(state::fnv1a(gb.framebuffer()), 0xECA4_7F65_4990_2B25);
    }

    #[test]
    fn version_1_layout_is_the_dmg_layout_of_today() {
        // Same machine, same point in time: the current writer's sections are
        // byte-identical to the v1 ones; only the header grew.
        let mut gb = GameBoy::new(v1_fixture_rom()).unwrap();
        for _ in 0..3 {
            gb.step_frame();
        }
        let now = gb.save_state();
        assert_eq!(&now[4..6], &state::VERSION.to_le_bytes());
        assert_eq!(
            &now[6..V1_HEADER_LEN],
            &STATE_V1[6..V1_HEADER_LEN],
            "ROM hash"
        );
        assert_eq!(&now[V1_HEADER_LEN..STATE_HEADER_LEN], &[0, 0], "DMG, DMG");
        assert!(now[STATE_HEADER_LEN..] == STATE_V1[V1_HEADER_LEN..]);
        // And a v1 state re-saves as the v2 state of the same machine.
        let mut other = GameBoy::new(v1_fixture_rom()).unwrap();
        other.load_state(STATE_V1).unwrap();
        assert!(other.save_state() == now);
    }

    #[test]
    fn version_1_states_do_not_load_into_a_cgb() {
        let mut gb = GameBoy::with_model(v1_fixture_rom(), Model::Cgb).unwrap();
        let before = gb.save_state();
        assert_eq!(gb.load_state(STATE_V1), Err(StateError::ModelMismatch));
        assert_eq!(gb.save_state(), before, "nothing changed");
        // A truncated or corrupt v1 state is still an error, and rolls back.
        let mut dmg = GameBoy::new(v1_fixture_rom()).unwrap();
        let before = dmg.save_state();
        assert_eq!(
            dmg.load_state(&STATE_V1[..STATE_V1.len() - 1]),
            Err(StateError::Truncated)
        );
        assert_eq!(dmg.save_state(), before);
        let mut extra = STATE_V1.to_vec();
        extra.push(0);
        assert_eq!(dmg.load_state(&extra), Err(StateError::TrailingBytes));
        assert_eq!(dmg.save_state(), before);
    }

    #[test]
    fn unknown_versions_are_still_rejected() {
        let mut gb = GameBoy::new(v1_fixture_rom()).unwrap();
        for v in [0u16, 3, 0xFFFF] {
            let mut s = STATE_V1.to_vec();
            s[4..6].copy_from_slice(&v.to_le_bytes());
            assert_eq!(gb.load_state(&s), Err(StateError::UnsupportedVersion(v)));
        }
    }

    #[test]
    fn the_hardware_mode_is_saved_range_checked_and_matched() {
        // (model, header flag, expected byte): DMG, CGB compat, CGB mode.
        for (model, flag, hw) in [
            (Model::Dmg, 0x80, 0),
            (Model::Cgb, 0x00, 1),
            (Model::Cgb, 0x80, 2),
        ] {
            let mut rom = nop_rom();
            rom[0x143] = flag;
            let mut gb = GameBoy::with_model(rom, model).unwrap();
            let snap = gb.save_state();
            assert_eq!(snap[STATE_HEADER_LEN - 1], hw);
            gb.load_state(&snap).unwrap();
            // Another valid mode (same ROM, same model) is refused, and so is an
            // out-of-range byte; neither touches the machine.
            for bad_hw in (0..=3).filter(|&b| b != hw) {
                let mut bad = snap.clone();
                bad[STATE_HEADER_LEN - 1] = bad_hw;
                assert_eq!(
                    gb.load_state(&bad),
                    Err(StateError::Invalid("hardware mode"))
                );
                assert_eq!(gb.save_state(), snap);
            }
        }
    }

    #[test]
    fn deterministic_audio_rate() {
        let mut gb = GameBoy::new(nop_rom()).unwrap();
        for _ in 0..60 {
            gb.step_frame();
        }
        let n = gb.audio_samples().len() / 2;
        // ~60 frames at 59.73 Hz → ~48200 samples at 48 kHz.
        assert!((48_000..48_500).contains(&n), "{n}");
    }
}
