//! `gba-core`: a deterministic, platform-independent Game Boy Advance emulator core.
//!
//! Like `gb-core`, it performs no I/O, reads no clocks and uses no randomness: the
//! same ROM + input sequence always produces the same frames and audio.
//!
//! Timing model (DECISIONS #45): one master clock in CPU cycles (16.78 MHz). The CPU
//! is stepped one instruction at a time and charges every bus access to the clock
//! through [`bus::CpuBus`]; each access costs the region's wait states for its
//! [`bus::Access`] kind. Everything else (PPU line timing, timers, DMA starts, the
//! sound FIFOs) is driven by cycle-stamped events on the [`scheduler::Scheduler`]
//! rather than ticked every cycle. Events that fall due during an instruction are
//! dispatched after it.
//!
//! Ownership of the modules (see the GBA plan): `cpu/` is the CPU agent's; `ppu/`
//! the PPU agent's; `bus`, `memory`, `dma`, `timer`, `irq`, `keypad`, `cart/` and
//! `gba` the system agent's; `apu/` and `bios/` come in phase P2. The types and
//! functions marked **Contract** are frozen; changing one is a contract change.
#![forbid(unsafe_code)]

pub mod apu;
pub mod bios;
pub mod bus;
pub mod cart;
pub mod cpu;
pub mod dma;
pub mod gba;
pub mod io;
pub mod irq;
pub mod keypad;
pub mod memory;
pub mod ppu;
pub mod scheduler;
pub mod timer;

pub use cart::CartError;
pub use gba::Gba;
pub use keypad::Button;

/// LCD width in pixels.
pub const SCREEN_WIDTH: usize = 240;
/// LCD height in pixels.
pub const SCREEN_HEIGHT: usize = 160;
/// One framebuffer, row-major. Each pixel is the palette-RAM colour
/// `0bbbbbgggggrrrrr` (red in the low 5 bits; bit 15 ignored), the same layout as a
/// CGB colour framebuffer, so hosts widen it with the same rule.
pub type Framebuffer = [u16; SCREEN_WIDTH * SCREEN_HEIGHT];

/// CPU clock in cycles per second (2^24).
pub const CYCLES_PER_SECOND: u32 = 16_777_216;
/// Cycles per scanline: 240 visible dots + 68 HBlank dots, 4 cycles per dot.
pub const CYCLES_PER_LINE: u32 = 1232;
/// Visible lines (VDraw).
pub const VISIBLE_LINES: u32 = 160;
/// Lines per frame: 160 VDraw + 68 VBlank.
pub const LINES_PER_FRAME: u32 = 228;
/// Cycles per frame (280 896).
pub const CYCLES_PER_FRAME: u32 = CYCLES_PER_LINE * LINES_PER_FRAME;
/// Native refresh rate, ~59.7275 Hz (the same as the Game Boy's).
pub const FRAMES_PER_SECOND: f64 = CYCLES_PER_SECOND as f64 / CYCLES_PER_FRAME as f64;

/// Largest ROM a GBA cartridge can map: 32 MiB (0x0800_0000..0x0A00_0000).
pub const MAX_ROM_SIZE: usize = 32 * 1024 * 1024;
/// The BIOS is exactly 16 KiB.
pub const BIOS_SIZE: usize = 16 * 1024;

/// FNV-1a 64 of a byte slice (ties save states and recordings to their ROM).
///
/// Same function as `gb-core`'s; it moves to `emu-common` when that crate lands.
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_timing_constants() {
        assert_eq!(CYCLES_PER_FRAME, 280_896);
        assert!((FRAMES_PER_SECOND - 59.7275).abs() < 0.001);
    }
}
