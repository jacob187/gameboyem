//! Save-state primitives shared by every core: a little-endian writer, a
//! bounds-checked reader and the error type. The layout of a state (magic,
//! version, sections) belongs to each core; see `gb_core::state`.
//!
//! State bytes are untrusted input: every read is bounds-checked and decoding
//! never panics. `f32`s are stored as their bit patterns so a round trip is
//! bit-exact.

use std::fmt;

/// Why a save state was rejected. The machine is unchanged when one is returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateError {
    /// Not a save state (wrong magic bytes).
    BadMagic,
    /// A save state from a newer or older, incompatible format version.
    UnsupportedVersion(u16),
    /// The state was taken with a different ROM.
    RomMismatch,
    /// The state was taken on a different hardware model (for a Game Boy, DMG vs CGB).
    ModelMismatch,
    /// The data ends before the state does.
    Truncated,
    /// A field holds a value the emulator can never be in.
    Invalid(&'static str),
    /// Extra bytes follow the last section.
    TrailingBytes,
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => write!(f, "not a save state"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported save-state version {v}"),
            Self::RomMismatch => write!(f, "save state is for a different ROM"),
            Self::ModelMismatch => write!(f, "save state is for a different hardware model"),
            Self::Truncated => write!(f, "save state is truncated"),
            Self::Invalid(what) => write!(f, "save state is corrupt ({what})"),
            Self::TrailingBytes => write!(f, "save state has trailing data"),
        }
    }
}

impl std::error::Error for StateError {}

/// 64-bit FNV-1a, used to tie a state to the ROM it was taken with.
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `Ok(())` if `ok`, else [`StateError::Invalid`] naming the field.
pub fn check(ok: bool, what: &'static str) -> Result<(), StateError> {
    if ok {
        Ok(())
    } else {
        Err(StateError::Invalid(what))
    }
}

#[derive(Default)]
pub struct StateWriter {
    buf: Vec<u8>,
}

impl StateWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// A writer with room for `capacity` bytes, to avoid regrowing the buffer.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    #[inline]
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    #[inline]
    pub fn bool(&mut self, v: bool) {
        self.buf.push(u8::from(v));
    }

    #[inline]
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    #[inline]
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    #[inline]
    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    #[inline]
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    #[inline]
    pub fn f32(&mut self, v: f32) {
        self.u32(v.to_bits());
    }

    /// Raw bytes; the reader must know the length.
    #[inline]
    pub fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }

    /// Length-prefixed bytes.
    #[inline]
    pub fn blob(&mut self, v: &[u8]) {
        let len = u32::try_from(v.len()).expect("state blob over 4 GiB");
        self.u32(len);
        self.bytes(v);
    }
}

pub struct StateReader<'a> {
    data: &'a [u8],
}

impl<'a> StateReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    #[inline]
    fn take(&mut self, n: usize) -> Result<&'a [u8], StateError> {
        if self.data.len() < n {
            return Err(StateError::Truncated);
        }
        let (head, rest) = self.data.split_at(n);
        self.data = rest;
        Ok(head)
    }

    #[inline]
    fn array<const N: usize>(&mut self) -> Result<[u8; N], StateError> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    #[inline]
    pub fn u8(&mut self) -> Result<u8, StateError> {
        Ok(self.take(1)?[0])
    }

    #[inline]
    pub fn bool(&mut self) -> Result<bool, StateError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(StateError::Invalid("bool")),
        }
    }

    #[inline]
    pub fn u16(&mut self) -> Result<u16, StateError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    #[inline]
    pub fn u32(&mut self) -> Result<u32, StateError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    #[inline]
    pub fn i32(&mut self) -> Result<i32, StateError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    #[inline]
    pub fn u64(&mut self) -> Result<u64, StateError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// A finite `f32`.
    #[inline]
    pub fn f32(&mut self) -> Result<f32, StateError> {
        let v = f32::from_bits(self.u32()?);
        check(v.is_finite(), "non-finite float")?;
        Ok(v)
    }

    /// Fill `out` with the next `out.len()` bytes.
    #[inline]
    pub fn bytes_into(&mut self, out: &mut [u8]) -> Result<(), StateError> {
        out.copy_from_slice(self.take(out.len())?);
        Ok(())
    }

    /// A length-prefixed blob that must be exactly `out.len()` bytes long.
    pub fn blob_into(&mut self, out: &mut [u8], what: &'static str) -> Result<(), StateError> {
        let len = self.u32()?;
        check(usize::try_from(len).is_ok_and(|l| l == out.len()), what)?;
        self.bytes_into(out)
    }

    /// Consume a section tag, failing if it isn't `expected`.
    #[inline]
    pub fn tag(&mut self, expected: u8) -> Result<(), StateError> {
        check(self.u8()? == expected, "section tag")
    }

    pub fn finish(self) -> Result<(), StateError> {
        if self.data.is_empty() {
            Ok(())
        } else {
            Err(StateError::TrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives_round_trip() {
        let mut w = StateWriter::new();
        w.u8(0xAB);
        w.bool(true);
        w.u16(0x1234);
        w.u32(0xDEAD_BEEF);
        w.i32(-7);
        w.u64(u64::MAX - 1);
        w.f32(-0.25);
        w.blob(&[1, 2, 3]);
        let bytes = w.into_bytes();
        let mut r = StateReader::new(&bytes);
        assert_eq!(r.u8(), Ok(0xAB));
        assert_eq!(r.bool(), Ok(true));
        assert_eq!(r.u16(), Ok(0x1234));
        assert_eq!(r.u32(), Ok(0xDEAD_BEEF));
        assert_eq!(r.i32(), Ok(-7));
        assert_eq!(r.u64(), Ok(u64::MAX - 1));
        assert_eq!(r.f32(), Ok(-0.25));
        let mut b = [0; 3];
        r.blob_into(&mut b, "blob").unwrap();
        assert_eq!(b, [1, 2, 3]);
        r.finish().unwrap();
    }

    #[test]
    fn short_reads_and_bad_values_are_errors() {
        assert_eq!(StateReader::new(&[1]).u16(), Err(StateError::Truncated));
        assert_eq!(
            StateReader::new(&[2]).bool(),
            Err(StateError::Invalid("bool"))
        );
        let nan = f32::NAN.to_bits().to_le_bytes();
        assert!(StateReader::new(&nan).f32().is_err());
        let mut b = [0; 2];
        let wrong_len = [3, 0, 0, 0, 1, 2, 3];
        assert!(StateReader::new(&wrong_len).blob_into(&mut b, "x").is_err());
        assert_eq!(
            StateReader::new(&[0]).finish(),
            Err(StateError::TrailingBytes)
        );
    }

    #[test]
    fn fnv1a_known_vectors() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
