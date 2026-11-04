//! `gb-core`: a deterministic, platform-independent Game Boy (DMG, and a growing
//! CGB mode) emulator core.
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
pub mod cartridge;
pub mod cpu;
pub mod dma;
pub mod gameboy;
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

/// CPU clock in T-cycles (dots) per second.
pub const T_CYCLES_PER_SECOND: u32 = 4_194_304;
/// T-cycles per frame (154 lines x 456 dots).
pub const T_CYCLES_PER_FRAME: u32 = 70_224;
/// M-cycles per frame.
pub const M_CYCLES_PER_FRAME: u32 = T_CYCLES_PER_FRAME / 4;
/// Native refresh rate, ~59.7275 Hz.
pub const FRAMES_PER_SECOND: f64 = T_CYCLES_PER_SECOND as f64 / T_CYCLES_PER_FRAME as f64;
