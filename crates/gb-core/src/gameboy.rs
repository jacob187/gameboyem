//! The public emulator facade.

use crate::bus::Bus;
use crate::cartridge::{self, CartridgeError, CartridgeHeader};
use crate::cpu::{Cpu, Registers};
use crate::joypad::Button;
use crate::{Framebuffer, M_CYCLES_PER_FRAME};

/// A complete DMG: CPU + bus (which owns every other component).
pub struct GameBoy {
    cpu: Cpu,
    bus: Bus,
}

impl GameBoy {
    /// Power on with `rom` inserted, in the DMG post-boot-ROM state.
    pub fn new(rom: Vec<u8>) -> Result<Self, CartridgeError> {
        let cart = cartridge::from_rom(rom)?;
        Ok(Self {
            cpu: Cpu::new(),
            bus: Bus::new(cart),
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

    /// Restore battery-backed RAM from a `.sav` file.
    pub fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        self.bus.cart.load_battery_ram(data)
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

    /// Side-effect-free memory read.
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
