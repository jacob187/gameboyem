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
//! The modules: `cpu/` (ARM7TDMI), `ppu/`, `apu/` (PSG channels and the two
//! Direct Sound FIFOs), `bios/` (the high-level BIOS), `cart/` (ROM, backup
//! chips, the GPIO port and its clock); the system pieces `bus`, `memory`, `io`,
//! `dma`, `timer`, `irq`, `keypad`, `prefetch` and `scheduler`; and `gba` (the
//! whole machine) with its `replay` and save-state (`state`) support. The types
//! and functions marked **Contract** are frozen; changing one is a contract
//! change.
#![forbid(unsafe_code)]

pub mod apu;
pub mod bios;
pub mod bus;
pub mod cart;
mod core_impl;
pub mod cpu;
pub mod dma;
pub mod gba;
pub mod io;
pub mod irq;
pub mod keypad;
pub mod memory;
pub mod ppu;
pub mod prefetch;
pub mod replay;
pub mod scheduler;
mod state;
pub mod timer;

pub use cart::{CartError, CartOptions, RtcMode, RtcTime, SaveChoice};
pub use gba::Gba;
pub use keypad::Button;
pub use state::StateError;

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

/// Upper bound on a save state, which a frontend uses to refuse an oversized file
/// unread. The state is fixed-size apart from a scheduler queue of at most 14
/// entries: EWRAM 256 KiB, IWRAM 32 KiB, VRAM 96 KiB, palette and OAM 2 KiB, two
/// 75 KiB frames and the save chip (at most 128 KiB of Flash) come to 681 175
/// bytes at worst (a full queue); the cap is that plus a little. A test builds
/// each chip's state and fails if the layout outgrows the cap or the cap drifts
/// well above the real size (DECISIONS #73).
pub const MAX_STATE_BYTES: usize = 680 * 1024;

/// FNV-1a 64 of a byte slice (ties save states and recordings to their ROM).
///
/// The same function as `gb-core`'s: it lives in `emu-common` and is re-exported
/// here so `gba_core::fnv1a` keeps working.
pub use emu_common::fnv1a;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_timing_constants() {
        assert_eq!(CYCLES_PER_FRAME, 280_896);
        assert!((FRAMES_PER_SECOND - 59.7275).abs() < 0.001);
    }

    #[test]
    fn fnv1a_is_the_shared_one() {
        // Known FNV-1a 64 vectors: the hash that tied ROMs to states before the
        // move to emu-common must not change.
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }
}
