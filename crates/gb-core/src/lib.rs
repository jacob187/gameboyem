//! `gb-core`: a deterministic, platform-independent Game Boy (DMG and CGB)
//! emulator core.
//!
//! The core performs no I/O, reads no clocks and uses no randomness, so the same
//! ROM + input sequence always produces the same frames, audio and serial output.
//!
//! Timing model (see `DECISIONS.md`): the CPU is stepped one instruction at a time,
//! and every memory access / internal cycle the instruction performs advances the
//! rest of the machine (timer, PPU, DMA, APU, serial) by one M-cycle through the
//! [`bus::CpuBus`] trait. This is "per instruction" stepping with the cycles
//! accounted at the exact M-cycle they occur, which `mem_timing` and the mooneye
//! timing tests require.
#![forbid(unsafe_code)]

pub mod apu;
pub mod bus;
mod bus_cgb;
pub mod cartridge;
pub mod cpu;
pub mod dma;
pub mod gameboy;
mod hdma;
pub mod interrupts;
pub mod joypad;
mod mmu;
pub mod model;
pub mod ppu;
pub mod serial;
pub mod state;
pub mod timer;

pub use cartridge::{Cartridge, CartridgeError, CartridgeHeader};
pub use cpu::Registers;
pub use gameboy::GameBoy;
pub use joypad::Button;
pub use model::Model;
pub use state::StateError;

/// LCD width in pixels.
pub const SCREEN_WIDTH: usize = 160;
/// LCD height in pixels.
pub const SCREEN_HEIGHT: usize = 144;
/// One framebuffer: `SCREEN_WIDTH * SCREEN_HEIGHT` shades, row-major.
/// Each byte is a DMG shade 0..=3 *after* palette mapping (0 = lightest, 3 = darkest).
pub type Framebuffer = [u8; SCREEN_WIDTH * SCREEN_HEIGHT];
/// One colour framebuffer, row-major. Each pixel is RGB555 as `0bbbbbgggggrrrrr`
/// (red in the low 5 bits, the hardware palette-RAM order).
pub type CgbFramebuffer = [u16; SCREEN_WIDTH * SCREEN_HEIGHT];

/// Widen one RGB555 pixel (`0bbbbbgggggrrrrr`; bit 15 is ignored) to RGB888. Each
/// 5-bit channel `c` becomes `(c << 3) | (c >> 2)`, with no colour correction (the
/// rule the cgb-acid2 reference image uses).
pub const fn rgb555_to_rgb888(c: u16) -> [u8; 3] {
    const fn widen(c: u16) -> u8 {
        let c = c & 0x1F;
        ((c << 3) | (c >> 2)) as u8
    }
    [widen(c), widen(c >> 5), widen(c >> 10)]
}

/// CPU clock in T-cycles (dots) per second.
pub const T_CYCLES_PER_SECOND: u32 = 4_194_304;
/// T-cycles per frame (154 lines x 456 dots).
pub const T_CYCLES_PER_FRAME: u32 = 70_224;
/// M-cycles per frame.
pub const M_CYCLES_PER_FRAME: u32 = T_CYCLES_PER_FRAME / 4;
/// Native refresh rate, ~59.7275 Hz.
pub const FRAMES_PER_SECOND: f64 = T_CYCLES_PER_SECOND as f64 / T_CYCLES_PER_FRAME as f64;

#[cfg(test)]
mod tests {
    use super::rgb555_to_rgb888;

    #[test]
    fn rgb555_expands_with_the_readme_rule() {
        assert_eq!(rgb555_to_rgb888(0x0000), [0, 0, 0]);
        assert_eq!(rgb555_to_rgb888(0x7FFF), [255, 255, 255]);
        // Red is the low 5 bits, blue the high.
        assert_eq!(rgb555_to_rgb888(0x001F), [255, 0, 0]);
        assert_eq!(rgb555_to_rgb888(0x03E0), [0, 255, 0]);
        assert_eq!(rgb555_to_rgb888(0x7C00), [0, 0, 255]);
        // (c << 3) | (c >> 2): 0x10 -> 0x84, 0x01 -> 0x08, 0x0F -> 0x7B.
        assert_eq!(rgb555_to_rgb888(0b01111_00001_10000), [0x84, 0x08, 0x7B]);
        // Bit 15 is ignored.
        assert_eq!(rgb555_to_rgb888(0x8000), [0, 0, 0]);
    }
}
