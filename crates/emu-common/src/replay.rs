//! The console-independent part of the input-recording format: header
//! framing, the 9-byte records and bounds-checked decoding. No I/O.
//!
//! A core's own crate wraps this with what it alone knows: how to power a
//! machine on from the header, what the `variant` byte means and how many
//! bytes of battery RAM are plausible.
//!
//! Layout (little-endian):
//!
//! ```text
//! header:  magic "GBINPUT\0" | version u8 | ROM FNV-1a u64 | battery length u32 | battery bytes
//!          | variant u8 (version 2 and later) | system u8 (version 3 and later)
//!          (battery length 0xFFFF_FFFF means "no battery RAM")
//! records: frame u64 | kind u8, repeated; 9 bytes each, append-only
//! ```
//!
//! Each version appends to the one before, so every earlier offset is
//! unchanged. A version 1 file has no variant byte and is a Game Boy DMG
//! (variant 0); a version 2 file has no system byte and is a Game Boy
//! (variant 0 DMG, 1 CGB). The variant is the hardware byte of the system's
//! save states (Game Boy: `Model::id`). The decoder keeps it as a raw byte;
//! the core's wrapper decides which values are valid.
//!
//! A record's `kind` is a button index plus `0x80` for a press, or `0x40`
//! (marker) or `0x41` (end). The index is the button's position in
//! [`Button::ALL`], so a system with fewer buttons rejects the indices it
//! lacks. A record's `frame` is the number of frames completed before it took
//! effect: a button change at frame `f` is applied before the `f+1`-th
//! [`Core::step_frame`], and a marker at frame `f` refers to the picture shown
//! after `f` frames. Records are in the order they happened, so a press and
//! release inside one frame both survive. The final `End` record holds the
//! total frame count; a recording cut short (crash, power loss) lacks it.

use std::fmt;

use crate::interface::{Button, Core, System};
use crate::state::fnv1a;

pub const MAGIC: [u8; 8] = *b"GBINPUT\0";
/// Version 2 added the hardware variant, version 3 the system. Older files
/// still decode.
pub const VERSION: u8 = 3;
pub const RECORD_LEN: usize = 9;

/// The most battery bytes [`decode_header`] accepts from any system. It is only
/// a sanity bound (so a hostile length cannot make us slice or copy far past the
/// file); each core then applies its own, smaller cap with [`check_battery`].
pub const MAX_BATTERY_BOUND: usize = 256 * 1024;

const NO_BATTERY: u32 = u32::MAX;
const KIND_PRESSED: u8 = 0x80;
const KIND_MARKER: u8 = 0x40;
const KIND_END: u8 = 0x41;

/// FNV-1a 64 of the ROM image; ties a recording to the ROM it was made with.
pub fn rom_hash(rom: &[u8]) -> u64 {
    fnv1a(rom)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Button {
        button: Button,
        pressed: bool,
    },
    /// The user flagged this frame (a screenshot was taken).
    Marker,
    /// The session ended; the frame is the total number of frames run.
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    pub frame: u64,
    pub event: Event,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub system: System,
    pub rom_hash: u64,
    /// `Core::battery_ram()` right after the session's save was loaded.
    pub battery: Option<Vec<u8>>,
    /// `Core::hw_variant()`, as a raw byte.
    pub variant: u8,
}

impl Header {
    pub fn encode(&self) -> Vec<u8> {
        encode_header(
            self.system,
            self.rom_hash,
            self.battery.as_deref(),
            self.variant,
        )
    }
}

/// Why a recording could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    NotARecording,
    UnsupportedVersion(u8),
    UnknownSystem(u8),
    TruncatedHeader,
    BatteryTooLarge { len: usize, max: usize },
    BadRecord { index: usize, kind: u8 },
    FramesOutOfOrder { index: usize },
    RecordAfterEnd { index: usize },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotARecording => write!(f, "not an input recording (bad magic)"),
            Self::UnsupportedVersion(v) => write!(
                f,
                "recording format version {v} is not supported (expected 1 to {VERSION})"
            ),
            Self::UnknownSystem(s) => write!(f, "recording has an unknown system byte {s:#04X}"),
            Self::TruncatedHeader => write!(f, "recording header is truncated"),
            Self::BatteryTooLarge { len, max } => {
                write!(f, "recorded battery RAM is {len} bytes (max {max})")
            }
            Self::BadRecord { index, kind } => {
                write!(f, "record {index} has an unknown kind {kind:#04X}")
            }
            Self::FramesOutOfOrder { index } => write!(f, "record {index} goes back in time"),
            Self::RecordAfterEnd { index } => write!(f, "record {index} follows the end record"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// The current-version header for a session on `system`.
pub fn encode_header(
    system: System,
    rom_hash: u64,
    battery: Option<&[u8]>,
    variant: u8,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(23 + battery.map_or(0, <[u8]>::len));
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&rom_hash.to_le_bytes());
    match battery {
        Some(b) => {
            let len = u32::try_from(b.len()).expect("battery RAM fits in u32");
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(b);
        }
        None => out.extend_from_slice(&NO_BATTERY.to_le_bytes()),
    }
    out.push(variant);
    out.push(system.id());
    out
}

pub fn encode_record(record: Record) -> [u8; RECORD_LEN] {
    let kind = match record.event {
        Event::Button { button, pressed } => {
            button.index() | if pressed { KIND_PRESSED } else { 0 }
        }
        Event::Marker => KIND_MARKER,
        Event::End => KIND_END,
    };
    let mut out = [0; RECORD_LEN];
    out[..8].copy_from_slice(&record.frame.to_le_bytes());
    out[8] = kind;
    out
}

/// Parse the header; returns it and the bytes after it (the records). The
/// input is untrusted. A battery image over [`MAX_BATTERY_BOUND`] is refused
/// here; the caller checks the header's system and then its own cap
/// ([`check_battery`]), so a recording of another system is reported as that,
/// not as having too large a battery for this one.
pub fn decode_header(bytes: &[u8]) -> Result<(Header, &[u8]), DecodeError> {
    if bytes.len() < MAGIC.len() || bytes[..MAGIC.len()] != MAGIC {
        return Err(DecodeError::NotARecording);
    }
    let rest = &bytes[MAGIC.len()..];
    let (&version, rest) = rest.split_first().ok_or(DecodeError::TruncatedHeader)?;
    if version == 0 || version > VERSION {
        return Err(DecodeError::UnsupportedVersion(version));
    }
    if rest.len() < 12 {
        return Err(DecodeError::TruncatedHeader);
    }
    let rom_hash = u64::from_le_bytes(rest[..8].try_into().expect("8 bytes"));
    let len = u32::from_le_bytes(rest[8..12].try_into().expect("4 bytes"));
    let rest = &rest[12..];
    let (battery, rest) = if len == NO_BATTERY {
        (None, rest)
    } else {
        let len = len as usize;
        if len > MAX_BATTERY_BOUND {
            return Err(DecodeError::BatteryTooLarge {
                len,
                max: MAX_BATTERY_BOUND,
            });
        }
        if rest.len() < len {
            return Err(DecodeError::TruncatedHeader);
        }
        (Some(rest[..len].to_vec()), &rest[len..])
    };
    let (variant, rest) = if version >= 2 {
        let (&v, rest) = rest.split_first().ok_or(DecodeError::TruncatedHeader)?;
        (v, rest)
    } else {
        (0, rest)
    };
    let (system, rest) = if version >= 3 {
        let (&id, rest) = rest.split_first().ok_or(DecodeError::TruncatedHeader)?;
        (
            System::from_id(id).ok_or(DecodeError::UnknownSystem(id))?,
            rest,
        )
    } else {
        (System::Gb, rest)
    };
    let header = Header {
        system,
        rom_hash,
        battery,
        variant,
    };
    Ok((header, rest))
}

/// Refuse a header whose battery image is over `max` bytes, the cap of the
/// system that is about to replay it.
pub fn check_battery(header: &Header, max: usize) -> Result<(), DecodeError> {
    match &header.battery {
        Some(b) if b.len() > max => Err(DecodeError::BatteryTooLarge { len: b.len(), max }),
        _ => Ok(()),
    }
}

/// The records of a recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Records {
    /// Every record except `End`, in file order.
    pub records: Vec<Record>,
    /// Total frames, from the `End` record; `None` if the session was cut short.
    pub end_frame: Option<u64>,
    /// The file ended inside a record (the partial record was dropped).
    pub truncated: bool,
}

fn decode_kind(kind: u8, buttons: usize) -> Option<Event> {
    match kind {
        KIND_MARKER => Some(Event::Marker),
        KIND_END => Some(Event::End),
        _ => {
            let index = kind & !KIND_PRESSED;
            if usize::from(index) >= buttons {
                return None;
            }
            Some(Event::Button {
                button: Button::from_index(index)?,
                pressed: kind & KIND_PRESSED != 0,
            })
        }
    }
}

/// Parse the records after a header. `buttons` is how many of
/// [`Button::ALL`] the system has ([`System::button_count`]); a record for
/// any other button is [`DecodeError::BadRecord`].
pub fn decode_records(rest: &[u8], buttons: usize) -> Result<Records, DecodeError> {
    let mut records = Vec::with_capacity(rest.len() / RECORD_LEN);
    let mut end_frame = None;
    let mut last_frame = 0;
    let mut chunks = rest.chunks_exact(RECORD_LEN);
    for (index, chunk) in chunks.by_ref().enumerate() {
        if end_frame.is_some() {
            return Err(DecodeError::RecordAfterEnd { index });
        }
        let frame = u64::from_le_bytes(chunk[..8].try_into().expect("8 bytes"));
        let kind = chunk[8];
        let event = decode_kind(kind, buttons).ok_or(DecodeError::BadRecord { index, kind })?;
        if frame < last_frame {
            return Err(DecodeError::FramesOutOfOrder { index });
        }
        last_frame = frame;
        match event {
            Event::End => end_frame = Some(frame),
            _ => records.push(Record { frame, event }),
        }
    }
    Ok(Records {
        records,
        end_frame,
        truncated: !chunks.remainder().is_empty(),
    })
}

/// Feeds recorded button changes into a core, frame by frame.
pub struct Replayer<'a> {
    records: &'a [Record],
    next: usize,
    frame: u64,
}

impl<'a> Replayer<'a> {
    pub fn new(records: &'a [Record]) -> Self {
        Self {
            records,
            next: 0,
            frame: 0,
        }
    }

    /// Frames run so far.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Records not yet applied.
    pub fn pending(&self) -> usize {
        self.records.len() - self.next
    }

    /// Apply the button changes due before the next frame, then run it.
    pub fn step_frame(&mut self, core: &mut dyn Core) {
        while let Some(r) = self.records.get(self.next) {
            if r.frame > self.frame {
                break;
            }
            if let Event::Button { button, pressed } = r.event {
                core.set_button(button, pressed);
            }
            self.next += 1;
        }
        core.step_frame();
        self.frame += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: u64 = 0x0123_4567_89AB_CDEF;
    const HASH_LE: [u8; 8] = [0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01];
    /// `magic | version | HASH | 3 | [1, 2, 3]`, the common prefix of every version.
    fn prefix(version: u8) -> Vec<u8> {
        let mut v = b"GBINPUT\0".to_vec();
        v.push(version);
        v.extend_from_slice(&HASH_LE);
        v.extend_from_slice(&[3, 0, 0, 0, 1, 2, 3]);
        v
    }

    /// Records: Start pressed at 0, Marker at 7, End at 12.
    const RECORDS: [u8; 27] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0x87, // frame 0, Start (index 7) pressed
        7, 0, 0, 0, 0, 0, 0, 0, 0x40, // frame 7, marker
        12, 0, 0, 0, 0, 0, 0, 0, 0x41, // frame 12, end
    ];
    fn expected_records() -> Vec<Record> {
        vec![
            Record {
                frame: 0,
                event: Event::Button {
                    button: Button::Start,
                    pressed: true,
                },
            },
            Record {
                frame: 7,
                event: Event::Marker,
            },
        ]
    }

    fn decode(bytes: &[u8], buttons: usize) -> Result<(Header, Records), DecodeError> {
        let (h, rest) = decode_header(bytes)?;
        Ok((h, decode_records(rest, buttons)?))
    }

    #[test]
    fn a_hand_built_version_1_file_is_a_game_boy_dmg() {
        let mut v = prefix(1);
        v.extend_from_slice(&RECORDS);
        let (h, r) = decode(&v, 8).unwrap();
        assert_eq!(h.system, System::Gb);
        assert_eq!(h.variant, 0);
        assert_eq!(h.rom_hash, HASH);
        assert_eq!(h.battery, Some(vec![1, 2, 3]));
        assert_eq!(r.records, expected_records());
        assert_eq!(r.end_frame, Some(12));
    }

    #[test]
    fn a_hand_built_version_2_file_keeps_its_variant_and_is_a_game_boy() {
        let mut v = prefix(2);
        v.push(1); // variant: CGB
        v.extend_from_slice(&RECORDS);
        let (h, r) = decode(&v, 8).unwrap();
        assert_eq!(h.system, System::Gb);
        assert_eq!(h.variant, 1);
        assert_eq!(h.battery, Some(vec![1, 2, 3]));
        assert_eq!(r.records, expected_records());
        assert_eq!(r.end_frame, Some(12));
    }

    #[test]
    fn a_hand_built_version_3_file_carries_its_system() {
        let mut v = prefix(3);
        v.extend_from_slice(&[0, 1]); // variant 0, system GBA
        v.extend_from_slice(&RECORDS);
        let (h, r) = decode(&v, 10).unwrap();
        assert_eq!(h.system, System::Gba);
        assert_eq!(h.variant, 0);
        assert_eq!(r.records, expected_records());
    }

    #[test]
    fn the_encoder_writes_version_3_with_every_earlier_offset_unchanged() {
        let bytes = encode_header(System::Gba, HASH, Some(&[1, 2, 3]), 0);
        let mut want = prefix(3);
        want.extend_from_slice(&[0, 1]);
        assert_eq!(bytes, want);
        let none = encode_header(System::Gb, HASH, None, 1);
        assert_eq!(&none[17..21], &[0xFF; 4], "battery length stays at 17..21");
        assert_eq!(&none[21..], &[1, 0], "variant, then system");
    }

    #[test]
    fn header_round_trips() {
        for system in [System::Gb, System::Gba] {
            for battery in [None, Some(vec![9; 40])] {
                let h = Header {
                    system,
                    rom_hash: 5,
                    battery,
                    variant: 1,
                };
                let bytes = h.encode();
                let (back, rest) = decode_header(&bytes).unwrap();
                assert_eq!(back, h);
                assert!(rest.is_empty());
            }
        }
    }

    #[test]
    fn rejects_a_bad_header() {
        let good = Header {
            system: System::Gb,
            rom_hash: HASH,
            battery: Some(vec![1, 2, 3]),
            variant: 0,
        }
        .encode();
        assert_eq!(decode_header(b"nope"), Err(DecodeError::NotARecording));
        let mut v = good.clone();
        v[8] = VERSION + 1;
        assert_eq!(
            decode_header(&v),
            Err(DecodeError::UnsupportedVersion(VERSION + 1))
        );
        v[8] = 0;
        assert_eq!(decode_header(&v), Err(DecodeError::UnsupportedVersion(0)));
        // Cut at every length short of the full header: always TruncatedHeader.
        for cut in 9..good.len() {
            assert_eq!(
                decode_header(&good[..cut]),
                Err(DecodeError::TruncatedHeader),
                "cut at {cut}"
            );
        }
        assert_eq!(
            check_battery(&decode_header(&good).unwrap().0, 2),
            Err(DecodeError::BatteryTooLarge { len: 3, max: 2 })
        );
        assert_eq!(check_battery(&decode_header(&good).unwrap().0, 3), Ok(()));
        // The sanity bound itself.
        let mut v = good.clone();
        v[17..21].copy_from_slice(&((MAX_BATTERY_BOUND + 1) as u32).to_le_bytes());
        assert_eq!(
            decode_header(&v),
            Err(DecodeError::BatteryTooLarge {
                len: MAX_BATTERY_BOUND + 1,
                max: MAX_BATTERY_BOUND
            })
        );
        let mut v = good.clone();
        *v.last_mut().unwrap() = 2;
        assert_eq!(decode_header(&v), Err(DecodeError::UnknownSystem(2)));
    }

    #[test]
    fn records_use_the_button_count() {
        for button in Button::ALL {
            for pressed in [false, true] {
                let r = Record {
                    frame: 3,
                    event: Event::Button { button, pressed },
                };
                let bytes = encode_record(r);
                let want_ok = System::Gba.has_button(button);
                assert_eq!(decode_records(&bytes, 10).is_ok(), want_ok);
                let on_gb = decode_records(&bytes, System::Gb.button_count());
                if System::Gb.has_button(button) {
                    assert_eq!(on_gb.unwrap().records, vec![r]);
                } else {
                    assert_eq!(
                        on_gb,
                        Err(DecodeError::BadRecord {
                            index: 0,
                            kind: bytes[8]
                        })
                    );
                }
            }
        }
    }

    #[test]
    fn marker_and_end_survive_any_button_count() {
        let marker = encode_record(Record {
            frame: 1,
            event: Event::Marker,
        });
        let end = encode_record(Record {
            frame: 2,
            event: Event::End,
        });
        for buttons in [0, 8, 10] {
            let mut v = marker.to_vec();
            v.extend_from_slice(&end);
            let r = decode_records(&v, buttons).unwrap();
            assert_eq!(r.records.len(), 1);
            assert_eq!(r.end_frame, Some(2));
        }
    }

    #[test]
    fn a_cut_short_session_keeps_the_complete_records() {
        let mut v = RECORDS[..18].to_vec();
        v.extend_from_slice(&[0xAA; 4]); // half a record
        let r = decode_records(&v, 8).unwrap();
        assert!(r.truncated);
        assert_eq!(r.records, expected_records());
        assert_eq!(r.end_frame, None);
    }

    #[test]
    fn rejects_malformed_records() {
        let mut v = RECORDS[..9].to_vec();
        v[8] = 0x3F;
        assert_eq!(
            decode_records(&v, 8),
            Err(DecodeError::BadRecord {
                index: 0,
                kind: 0x3F
            })
        );
        let mut v = RECORDS.to_vec();
        v.extend_from_slice(&encode_record(Record {
            frame: 12,
            event: Event::Marker,
        }));
        assert_eq!(
            decode_records(&v, 8),
            Err(DecodeError::RecordAfterEnd { index: 3 })
        );
        let mut v = RECORDS[..18].to_vec();
        v.extend_from_slice(&encode_record(Record {
            frame: 1,
            event: Event::Marker,
        }));
        assert_eq!(
            decode_records(&v, 8),
            Err(DecodeError::FramesOutOfOrder { index: 2 })
        );
    }

    #[test]
    fn rom_hash_is_fnv1a() {
        assert_eq!(rom_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(rom_hash(b""), 0xcbf2_9ce4_8422_2325);
    }
}
