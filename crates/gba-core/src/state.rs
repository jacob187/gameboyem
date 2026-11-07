//! Save states: a versioned binary snapshot of the whole machine (DECISIONS #73).
//!
//! Layout: magic `GBAS`, a `u16` format version, the FNV-1a hash of the ROM, a
//! BIOS kind byte (0 built-in HLE, 1 a real dump) with the FNV-1a hash of the
//! dump (0 for HLE), the cartridge options (`SaveChoice` and `RtcMode` ids),
//! then one tagged section per component in a fixed order (see
//! [`crate::Gba::save_state`]). All integers are little-endian.
//!
//! State bytes are untrusted input: every read is bounds-checked and every
//! field that a component later uses as an index, a loop bound, a divisor or a
//! counter that must not underflow is range-checked against what the emulator
//! can reach. A check rejects only values the emulator can never produce; decoding
//! never panics. Components validate what they read before they assign it, but
//! the bus assigns its parts one after another and checks the relations between
//! them last, so a failed load can leave a component half written. The load is
//! all-or-nothing because [`crate::Gba::load_state`] loads into a scratch machine
//! and replaces `self` only when every section and relation checked out.
//!
//! Host-side bookkeeping is not part of a state: the audio sample rate, the
//! buffered audio samples and the battery-save generation counter.

use emu_common::state::{StateReader, StateWriter};

pub(crate) const MAGIC: [u8; 4] = *b"GBAS";
pub(crate) const VERSION: u16 = 1;
/// Bytes before the first section: magic, version, ROM hash, BIOS kind and hash,
/// save choice, RTC mode.
#[cfg(test)]
pub(crate) const HEADER_LEN: usize = 4 + 2 + 8 + 1 + 8 + 1 + 1;

/// Section tags, in file order.
pub(crate) mod tag {
    pub const CPU: u8 = 1;
    pub const SCHEDULER: u8 = 2;
    pub const BUS: u8 = 3;
    pub const IRQ: u8 = 4;
    pub const TIMERS: u8 = 5;
    pub const DMA: u8 = 6;
    pub const PPU: u8 = 7;
    pub const APU: u8 = 8;
    pub const KEYPAD: u8 = 9;
    pub const PREFETCH: u8 = 10;
    pub const CART: u8 = 11;
    pub const RTC_START: u8 = 12;
}

// The byte-level primitives live in `emu-common`.
pub use emu_common::state::{StateError, check};

/// Upper bound on any clock-like counter in a state (the master clock, the
/// scheduler's sequence number, timestamps): 2^62 cycles is over 8000 years of
/// emulated time, and keeps every `at + delay` sum far from overflow.
pub(crate) const MAX_CLOCK: u64 = 1 << 62;

pub(crate) fn put_u16s(w: &mut StateWriter, v: &[u16]) {
    for &x in v {
        w.u16(x);
    }
}

pub(crate) fn get_u16s(r: &mut StateReader, out: &mut [u16]) -> Result<(), StateError> {
    for x in out {
        *x = r.u16()?;
    }
    Ok(())
}

pub(crate) fn put_u32s(w: &mut StateWriter, v: &[u32]) {
    for &x in v {
        w.u32(x);
    }
}

pub(crate) fn get_u32s<const N: usize>(r: &mut StateReader) -> Result<[u32; N], StateError> {
    let mut out = [0; N];
    for x in &mut out {
        *x = r.u32()?;
    }
    Ok(out)
}
