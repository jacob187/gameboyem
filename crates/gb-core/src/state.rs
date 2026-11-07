//! Save states: a versioned binary snapshot of the whole machine.
//!
//! Layout: magic `GBST`, a `u16` format version, the FNV-1a hash of the ROM
//! the state was taken with, the model (`u8`: 0 DMG, 1 CGB), the hardware mode
//! (`u8`: 0 DMG, 1 CGB in compatibility mode, 2 CGB), then one tagged section
//! per component in a fixed order (see [`crate::GameBoy::save_state`]). All
//! integers are little-endian; `f32`s are stored as their bit patterns so a
//! round trip is bit-exact. Version 1 has neither the model nor the hardware
//! byte (it predates the CGB; its sections are the DMG ones) and loads as a DMG.
//!
//! State bytes are untrusted input: every read is bounds-checked and every
//! field that a component later uses as an index, a loop bound or a counter
//! that must not underflow is range-checked against what the hardware model
//! can reach. Decoding never panics.
//!
//! Host-side bookkeeping is not part of a state: the audio sample rate and its
//! filter factor, buffered audio samples, captured serial output and the
//! battery-save generation counter.

pub(crate) const MAGIC: [u8; 4] = *b"GBST";
pub(crate) const VERSION: u16 = 2;

/// Section tags, in file order.
pub(crate) mod tag {
    pub const CPU: u8 = 1;
    pub const BUS: u8 = 2;
    pub const INTERRUPTS: u8 = 3;
    pub const TIMER: u8 = 4;
    pub const PPU: u8 = 5;
    pub const DMA: u8 = 6;
    pub const APU: u8 = 7;
    pub const JOYPAD: u8 = 8;
    pub const SERIAL: u8 = 9;
    pub const CART: u8 = 10;
}

// The byte-level primitives live in `emu-common`; re-exported so every
// `crate::state::X` path (and `gb_core::state::X` for hosts) is unchanged.
pub use emu_common::state::{StateError, StateReader, StateWriter, check, fnv1a};
