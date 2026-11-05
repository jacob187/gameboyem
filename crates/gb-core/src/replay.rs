//! Input recordings: everything needed to re-run a play session exactly.
//!
//! The core is deterministic, so a session is fully described by the ROM, the
//! battery RAM it powered on with, and the ordered joypad changes keyed by
//! frame. This module only encodes and decodes bytes (no I/O); the frontend
//! writes recordings and `gb-test --replay` plays them back.
//!
//! Layout (little-endian):
//!
//! ```text
//! header:  magic "GBINPUT\0" | version u8 | ROM FNV-1a u64 | battery length u32 | battery bytes
//!          | model u8 (version 2 and later)
//!          (battery length 0xFFFF_FFFF means "no battery RAM")
//! records: frame u64 | kind u8, repeated; 9 bytes each, append-only
//! ```
//!
//! Version 2 appends the hardware model after the version-1 fields, so every
//! earlier offset is unchanged. The byte is the one a save state stores
//! (`Model::id`: 0 DMG, 1 CGB). A version 1 file has no model byte and
//! replays as a DMG, which is all that existed when it was written.
//!
//! A record's `frame` is the number of frames completed before it took effect:
//! a button change at frame `f` is applied before the `f+1`-th
//! [`GameBoy::step_frame`], and a marker at frame `f` refers to the picture
//! shown after `f` frames. Records are in the order they happened, so a press
//! and release inside one frame both survive. The final `End` record holds the
//! total frame count; a recording cut short (crash, power loss) lacks it.

use crate::{Button, CartridgeError, GameBoy, Model};

pub const MAGIC: [u8; 8] = *b"GBINPUT\0";
/// Version 2 adds the hardware model. Version 1 files still decode (as a DMG).
pub const VERSION: u8 = 2;
/// Largest battery image any supported cartridge produces (128 KiB RAM + RTC footer).
pub const MAX_BATTERY_LEN: usize = 128 * 1024 + 48;
pub const RECORD_LEN: usize = 9;

const NO_BATTERY: u32 = u32::MAX;
const KIND_PRESSED: u8 = 0x80;
const KIND_MARKER: u8 = 0x40;
const KIND_END: u8 = 0x41;

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
    pub rom_hash: u64,
    /// `GameBoy::battery_ram()` right after the session's save was loaded.
    pub battery: Option<Vec<u8>>,
    /// The console the session ran on; a version 1 file decodes as [`Model::Dmg`].
    pub model: Model,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recording {
    pub header: Header,
    /// Every record except `End`, in file order.
    pub records: Vec<Record>,
    /// Total frames, from the `End` record; `None` if the session was cut short.
    pub end_frame: Option<u64>,
    /// The file ended inside a record (the partial record was dropped).
    pub truncated: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplayError {
    NotARecording,
    UnsupportedVersion(u8),
    BadModel(u8),
    TruncatedHeader,
    BatteryTooLarge(usize),
    BadRecord { index: usize, kind: u8 },
    FramesOutOfOrder { index: usize },
    RecordAfterEnd { index: usize },
    WrongRom { expected: u64, actual: u64 },
    Cartridge(CartridgeError),
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotARecording => write!(f, "not an input recording (bad magic)"),
            Self::UnsupportedVersion(v) => {
                write!(
                    f,
                    "recording format version {v} is not supported (expected 1 or {VERSION})"
                )
            }
            Self::BadModel(m) => write!(f, "recording has an unknown hardware model byte {m:#04X}"),
            Self::TruncatedHeader => write!(f, "recording header is truncated"),
            Self::BatteryTooLarge(n) => {
                write!(
                    f,
                    "recorded battery RAM is {n} bytes (max {MAX_BATTERY_LEN})"
                )
            }
            Self::BadRecord { index, kind } => {
                write!(f, "record {index} has an unknown kind {kind:#04X}")
            }
            Self::FramesOutOfOrder { index } => {
                write!(f, "record {index} goes back in time")
            }
            Self::RecordAfterEnd { index } => write!(f, "record {index} follows the end record"),
            Self::WrongRom { expected, actual } => write!(
                f,
                "recording was made with a different ROM (hash {expected:016x}, this ROM {actual:016x})"
            ),
            Self::Cartridge(e) => write!(f, "cannot start the recording: {e}"),
        }
    }
}

impl std::error::Error for ReplayError {}

/// FNV-1a 64 of the ROM image; ties a recording to the ROM it was made with.
pub fn rom_hash(rom: &[u8]) -> u64 {
    emu_common::fnv1a(rom)
}

pub fn encode_header(header: &Header) -> Vec<u8> {
    let battery = header.battery.as_deref();
    let mut out = Vec::with_capacity(22 + battery.map_or(0, <[u8]>::len));
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&header.rom_hash.to_le_bytes());
    match battery {
        Some(b) => {
            let len = u32::try_from(b.len()).expect("battery RAM fits in u32");
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(b);
        }
        None => out.extend_from_slice(&NO_BATTERY.to_le_bytes()),
    }
    out.push(header.model.id());
    out
}

pub fn encode_record(record: Record) -> [u8; RECORD_LEN] {
    let kind = match record.event {
        Event::Button { button, pressed } => {
            let index = Button::ALL
                .iter()
                .position(|&b| b == button)
                .expect("Button::ALL lists every button") as u8;
            index | if pressed { KIND_PRESSED } else { 0 }
        }
        Event::Marker => KIND_MARKER,
        Event::End => KIND_END,
    };
    let mut out = [0; RECORD_LEN];
    out[..8].copy_from_slice(&record.frame.to_le_bytes());
    out[8] = kind;
    out
}

fn decode_kind(kind: u8) -> Option<Event> {
    match kind {
        KIND_MARKER => Some(Event::Marker),
        KIND_END => Some(Event::End),
        _ if kind & !KIND_PRESSED < 8 => Some(Event::Button {
            button: Button::ALL[usize::from(kind & !KIND_PRESSED)],
            pressed: kind & KIND_PRESSED != 0,
        }),
        _ => None,
    }
}

impl Recording {
    /// Parse a recording. The input is untrusted: every length and value is checked.
    pub fn decode(bytes: &[u8]) -> Result<Self, ReplayError> {
        if bytes.len() < MAGIC.len() || bytes[..MAGIC.len()] != MAGIC {
            return Err(ReplayError::NotARecording);
        }
        let rest = &bytes[MAGIC.len()..];
        let (&version, rest) = rest.split_first().ok_or(ReplayError::TruncatedHeader)?;
        if version == 0 || version > VERSION {
            return Err(ReplayError::UnsupportedVersion(version));
        }
        if rest.len() < 12 {
            return Err(ReplayError::TruncatedHeader);
        }
        let rom_hash = u64::from_le_bytes(rest[..8].try_into().expect("8 bytes"));
        let len = u32::from_le_bytes(rest[8..12].try_into().expect("4 bytes"));
        let rest = &rest[12..];
        let (battery, rest) = if len == NO_BATTERY {
            (None, rest)
        } else {
            let len = len as usize;
            if len > MAX_BATTERY_LEN {
                return Err(ReplayError::BatteryTooLarge(len));
            }
            if rest.len() < len {
                return Err(ReplayError::TruncatedHeader);
            }
            (Some(rest[..len].to_vec()), &rest[len..])
        };
        let (model, rest) = if version >= 2 {
            let (&id, rest) = rest.split_first().ok_or(ReplayError::TruncatedHeader)?;
            (Model::from_id(id).ok_or(ReplayError::BadModel(id))?, rest)
        } else {
            (Model::Dmg, rest)
        };

        let mut records = Vec::with_capacity(rest.len() / RECORD_LEN);
        let mut end_frame = None;
        let mut last_frame = 0;
        let mut chunks = rest.chunks_exact(RECORD_LEN);
        for (index, chunk) in chunks.by_ref().enumerate() {
            if end_frame.is_some() {
                return Err(ReplayError::RecordAfterEnd { index });
            }
            let frame = u64::from_le_bytes(chunk[..8].try_into().expect("8 bytes"));
            let kind = chunk[8];
            let event = decode_kind(kind).ok_or(ReplayError::BadRecord { index, kind })?;
            if frame < last_frame {
                return Err(ReplayError::FramesOutOfOrder { index });
            }
            last_frame = frame;
            match event {
                Event::End => end_frame = Some(frame),
                _ => records.push(Record { frame, event }),
            }
        }
        Ok(Self {
            header: Header {
                rom_hash,
                battery,
                model,
            },
            records,
            end_frame,
            truncated: !chunks.remainder().is_empty(),
        })
    }

    /// Frames flagged with a marker, in order (duplicates removed).
    pub fn markers(&self) -> Vec<u64> {
        let mut out: Vec<u64> = self
            .records
            .iter()
            .filter(|r| r.event == Event::Marker)
            .map(|r| r.frame)
            .collect();
        out.dedup();
        out
    }

    /// Frames to run for a full replay: the recorded total, or (if the session
    /// was cut short) up to the last record.
    pub fn frame_count(&self) -> u64 {
        self.end_frame
            .unwrap_or_else(|| self.records.last().map_or(0, |r| r.frame))
    }

    /// Power on a machine in the state the recorded session started from.
    pub fn power_on(&self, rom: Vec<u8>) -> Result<GameBoy, ReplayError> {
        let actual = rom_hash(&rom);
        if actual != self.header.rom_hash {
            return Err(ReplayError::WrongRom {
                expected: self.header.rom_hash,
                actual,
            });
        }
        let mut gb = GameBoy::with_model(rom, self.header.model).map_err(ReplayError::Cartridge)?;
        if let Some(battery) = &self.header.battery {
            gb.load_battery_ram(battery)
                .map_err(ReplayError::Cartridge)?;
        }
        Ok(gb)
    }
}

/// Feeds a recording's button changes into a machine, frame by frame.
pub struct Replayer<'a> {
    records: &'a [Record],
    next: usize,
    frame: u64,
}

impl<'a> Replayer<'a> {
    pub fn new(recording: &'a Recording) -> Self {
        Self {
            records: &recording.records,
            next: 0,
            frame: 0,
        }
    }

    /// Frames run so far.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Apply the button changes due before the next frame, then run it.
    pub fn step_frame(&mut self, gb: &mut GameBoy) {
        while let Some(r) = self.records.get(self.next) {
            if r.frame > self.frame {
                break;
            }
            if let Event::Button { button, pressed } = r.event {
                gb.set_button(button, pressed);
            }
            self.next += 1;
        }
        gb.step_frame();
        self.frame += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Header, Vec<Record>) {
        let header = Header {
            rom_hash: 0x0123_4567_89AB_CDEF,
            battery: Some(vec![1, 2, 3]),
            model: Model::Cgb,
        };
        let records = vec![
            Record {
                frame: 0,
                event: Event::Button {
                    button: Button::Start,
                    pressed: true,
                },
            },
            Record {
                frame: 0,
                event: Event::Button {
                    button: Button::Start,
                    pressed: false,
                },
            },
            Record {
                frame: 7,
                event: Event::Marker,
            },
            Record {
                frame: 9,
                event: Event::Button {
                    button: Button::Right,
                    pressed: true,
                },
            },
        ];
        (header, records)
    }

    fn encode(header: &Header, records: &[Record], end: Option<u64>) -> Vec<u8> {
        let mut out = encode_header(header);
        for &r in records {
            out.extend_from_slice(&encode_record(r));
        }
        if let Some(frame) = end {
            out.extend_from_slice(&encode_record(Record {
                frame,
                event: Event::End,
            }));
        }
        out
    }

    #[test]
    fn round_trip() {
        let (header, records) = sample();
        let rec = Recording::decode(&encode(&header, &records, Some(12))).unwrap();
        assert_eq!(rec.header, header);
        assert_eq!(rec.records, records);
        assert_eq!(rec.end_frame, Some(12));
        assert_eq!(rec.frame_count(), 12);
        assert!(!rec.truncated);
        assert_eq!(rec.markers(), vec![7]);
    }

    #[test]
    fn every_button_and_state_round_trips() {
        for button in Button::ALL {
            for pressed in [false, true] {
                let r = Record {
                    frame: 3,
                    event: Event::Button { button, pressed },
                };
                let bytes = encode(
                    &Header {
                        rom_hash: 0,
                        battery: None,
                        model: Model::Dmg,
                    },
                    &[r],
                    None,
                );
                assert_eq!(Recording::decode(&bytes).unwrap().records, vec![r]);
            }
        }
    }

    /// Offset of the model byte in `sample()`'s header (3 battery bytes).
    const MODEL_AT: usize = 8 + 1 + 8 + 4 + 3;

    #[test]
    fn model_round_trips() {
        for model in [Model::Dmg, Model::Cgb] {
            let header = Header {
                rom_hash: 1,
                battery: None,
                model,
            };
            let rec = Recording::decode(&encode(&header, &[], Some(0))).unwrap();
            assert_eq!(rec.header.model, model);
        }
    }

    #[test]
    fn version_1_decodes_as_dmg() {
        // A v1 file: version byte 1 and no model byte after the battery.
        let (header, records) = sample();
        let mut v = encode(&header, &records, Some(12));
        v[8] = 1;
        v.remove(MODEL_AT);
        let rec = Recording::decode(&v).unwrap();
        assert_eq!(rec.header.model, Model::Dmg);
        assert_eq!(rec.header.battery, header.battery);
        assert_eq!(rec.records, records);
        assert_eq!(rec.end_frame, Some(12));
    }

    #[test]
    fn rejects_an_unknown_model_byte() {
        let (header, records) = sample();
        let good = encode(&header, &records, Some(12));
        assert_eq!(good[MODEL_AT], Model::Cgb.id());
        for bad in [2, 0x7F, 0xFF] {
            let mut v = good.clone();
            v[MODEL_AT] = bad;
            assert_eq!(Recording::decode(&v), Err(ReplayError::BadModel(bad)));
        }
    }

    #[test]
    fn cut_short_session_keeps_complete_records() {
        let (header, records) = sample();
        let mut bytes = encode(&header, &records, None);
        bytes.extend_from_slice(&[0xAA; 4]); // half a record
        let rec = Recording::decode(&bytes).unwrap();
        assert!(rec.truncated);
        assert_eq!(rec.records, records);
        assert_eq!(rec.end_frame, None);
        assert_eq!(rec.frame_count(), 9, "up to the last record");
    }

    #[test]
    fn rejects_malformed_input() {
        let (header, records) = sample();
        let good = encode(&header, &records, Some(12));
        assert_eq!(Recording::decode(b"nope"), Err(ReplayError::NotARecording));
        let mut v = good.clone();
        v[8] = VERSION + 1;
        assert_eq!(
            Recording::decode(&v),
            Err(ReplayError::UnsupportedVersion(VERSION + 1))
        );
        v[8] = 0;
        assert_eq!(
            Recording::decode(&v),
            Err(ReplayError::UnsupportedVersion(0))
        );
        assert_eq!(
            Recording::decode(&good[..15]),
            Err(ReplayError::TruncatedHeader)
        );
        assert_eq!(
            Recording::decode(&good[..22]),
            Err(ReplayError::TruncatedHeader)
        );
        // The battery is complete but the model byte is missing.
        assert_eq!(
            Recording::decode(&good[..MODEL_AT]),
            Err(ReplayError::TruncatedHeader)
        );

        let mut v = good.clone();
        v[17..21].copy_from_slice(&((MAX_BATTERY_LEN + 1) as u32).to_le_bytes());
        assert_eq!(
            Recording::decode(&v),
            Err(ReplayError::BatteryTooLarge(MAX_BATTERY_LEN + 1))
        );

        let first = encode_header(&header).len();
        let mut v = good.clone();
        v[first + 8] = 0x3F;
        assert_eq!(
            Recording::decode(&v),
            Err(ReplayError::BadRecord {
                index: 0,
                kind: 0x3F
            })
        );

        let mut v = encode(&header, &records, None);
        v.extend_from_slice(&encode_record(Record {
            frame: 1,
            event: Event::Marker,
        }));
        assert_eq!(
            Recording::decode(&v),
            Err(ReplayError::FramesOutOfOrder { index: 4 })
        );

        let mut v = good;
        v.extend_from_slice(&encode_record(Record {
            frame: 12,
            event: Event::Marker,
        }));
        assert_eq!(
            Recording::decode(&v),
            Err(ReplayError::RecordAfterEnd { index: 5 })
        );
    }

    #[test]
    fn power_on_checks_the_rom() {
        let mut rom = vec![0u8; 0x8000];
        rom[0x147] = 0x00;
        let rec = Recording {
            header: Header {
                rom_hash: rom_hash(&rom),
                battery: None,
                model: Model::Dmg,
            },
            records: Vec::new(),
            end_frame: Some(0),
            truncated: false,
        };
        assert!(rec.power_on(rom.clone()).is_ok());
        rom[0x200] = 1;
        assert!(matches!(
            rec.power_on(rom),
            Err(ReplayError::WrongRom { .. })
        ));
    }

    #[test]
    fn power_on_uses_the_recorded_model() {
        let mut rom = vec![0u8; 0x8000];
        rom[0x143] = 0xC0; // CGB only
        for model in [Model::Dmg, Model::Cgb] {
            let rec = Recording {
                header: Header {
                    rom_hash: rom_hash(&rom),
                    battery: None,
                    model,
                },
                records: Vec::new(),
                end_frame: Some(1),
                truncated: false,
            };
            let mut gb = rec.power_on(rom.clone()).unwrap();
            gb.step_frame();
            assert_eq!(gb.model(), model);
            assert_eq!(gb.cgb_framebuffer().is_some(), model == Model::Cgb);
        }
    }

    #[test]
    fn replayer_applies_events_before_their_frame() {
        let mut rom = vec![0u8; 0x8000];
        rom[0x147] = 0x00;
        let rec = Recording {
            header: Header {
                rom_hash: rom_hash(&rom),
                battery: None,
                model: Model::Dmg,
            },
            records: vec![Record {
                frame: 2,
                event: Event::Button {
                    button: Button::A,
                    pressed: true,
                },
            }],
            end_frame: Some(4),
            truncated: false,
        };
        let mut gb = rec.power_on(rom).unwrap();
        let mut player = Replayer::new(&rec);
        for _ in 0..4 {
            player.step_frame(&mut gb);
        }
        assert_eq!(player.frame(), 4);
        assert_eq!(player.next, 1, "the press was consumed");
    }
}
