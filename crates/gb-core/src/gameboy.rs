//! The public emulator facade.

use crate::bus::Bus;
use crate::cartridge::{self, CartridgeError, CartridgeHeader};
use crate::cpu::{Cpu, Registers};
use crate::joypad::Button;
use crate::state::{self, StateError, StateReader, StateWriter, tag};
use crate::{Framebuffer, M_CYCLES_PER_FRAME};

/// Magic + version + ROM hash.
const STATE_HEADER_LEN: usize = 4 + 2 + 8;

/// A complete DMG: CPU + bus (which owns every other component).
pub struct GameBoy {
    cpu: Cpu,
    bus: Bus,
    /// FNV-1a of the ROM image; ties save states to the ROM they came from.
    rom_hash: u64,
}

impl GameBoy {
    /// Power on with `rom` inserted, in the DMG post-boot-ROM state.
    pub fn new(rom: Vec<u8>) -> Result<Self, CartridgeError> {
        let rom_hash = state::fnv1a(&rom);
        let cart = cartridge::from_rom(rom)?;
        Ok(Self {
            cpu: Cpu::new(),
            bus: Bus::new(cart),
            rom_hash,
        })
    }

    /// Replace the cartridge and power-cycle the machine.
    ///
    /// Host configuration survives (the audio sample rate set by
    /// [`GameBoy::set_audio_sample_rate`]); all emulated state is reset,
    /// including held buttons, which the host must re-press. On error the
    /// machine is left untouched.
    pub fn load_rom(&mut self, rom: Vec<u8>) -> Result<(), CartridgeError> {
        let mut fresh = Self::new(rom)?;
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

    /// Last completed frame: 160x144 shades 0..=3 (0 = lightest).
    pub fn framebuffer(&self) -> &Framebuffer {
        self.bus.ppu.framebuffer()
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
        w.u8(tag::CPU);
        self.cpu.save_state(&mut w);
        self.bus.save_state(&mut w);
        w.into_bytes()
    }

    /// Restore a snapshot from [`GameBoy::save_state`] taken with the same ROM.
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
        if version != state::VERSION {
            return Err(StateError::UnsupportedVersion(version));
        }
        if r.u64()? != self.rom_hash {
            return Err(StateError::RomMismatch);
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
