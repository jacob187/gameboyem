//! Input recordings: everything needed to re-run a play session exactly.
//!
//! The core is deterministic, so a session is fully described by the ROM, the
//! battery RAM it powered on with, and the ordered joypad changes keyed by
//! frame. The byte format (header framing, records, decoding) is shared with
//! the other cores and lives in [`emu_common::replay`]; its layout and the
//! frame-numbering rules are documented there. This module adds what is
//! Game Boy specific: the hardware model carried in the header's variant byte,
//! the 8-button limit, the battery size cap and powering a [`GameBoy`] on from
//! a recording. It only encodes and decodes bytes (no I/O); the frontend
//! writes recordings and `gb-test --replay` plays them back.
//!
//! The variant byte is `Model::id` (0 DMG, 1 CGB). A version 1 file has none
//! and replays as a DMG, which is all that existed when it was written; a
//! version 2 file has no system byte and is a Game Boy by definition. Version
//! 3 adds the system byte, and this decoder refuses a recording of another
//! system.

use emu_common::System;
use emu_common::replay::{self as common, DecodeError};

use crate::{CartridgeError, GameBoy, Model};

pub use emu_common::replay::{Event, MAGIC, RECORD_LEN, Record, VERSION, encode_record, rom_hash};

/// Largest battery image any supported cartridge produces (128 KiB RAM + RTC footer).
pub const MAX_BATTERY_LEN: usize = 128 * 1024 + 48;

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
    /// The shared format rejected the file (bad magic, version, record, ...).
    Decode(DecodeError),
    BadModel(u8),
    /// A well-formed recording of a console this core does not emulate.
    WrongSystem(System),
    WrongRom {
        expected: u64,
        actual: u64,
    },
    Cartridge(CartridgeError),
}

impl From<DecodeError> for ReplayError {
    fn from(e: DecodeError) -> Self {
        Self::Decode(e)
    }
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "{e}"),
            Self::BadModel(m) => write!(f, "recording has an unknown hardware model byte {m:#04X}"),
            Self::WrongSystem(s) => write!(f, "recording is for a {s:?} machine, not a Game Boy"),
            Self::WrongRom { expected, actual } => write!(
                f,
                "recording was made with a different ROM (hash {expected:016x}, this ROM {actual:016x})"
            ),
            Self::Cartridge(e) => write!(f, "cannot start the recording: {e}"),
        }
    }
}

impl std::error::Error for ReplayError {}

pub fn encode_header(header: &Header) -> Vec<u8> {
    common::encode_header(
        System::Gb,
        header.rom_hash,
        header.battery.as_deref(),
        header.model.id(),
    )
}

impl Recording {
    /// Parse a recording. The input is untrusted: every length and value is checked.
    pub fn decode(bytes: &[u8]) -> Result<Self, ReplayError> {
        let (header, rest) = common::decode_header(bytes)?;
        if header.system != System::Gb {
            return Err(ReplayError::WrongSystem(header.system));
        }
        common::check_battery(&header, MAX_BATTERY_LEN)?;
        let model = Model::from_id(header.variant).ok_or(ReplayError::BadModel(header.variant))?;
        let records = common::decode_records(rest, System::Gb.button_count())?;
        Ok(Self {
            header: Header {
                rom_hash: header.rom_hash,
                battery: header.battery,
                model,
            },
            records: records.records,
            end_frame: records.end_frame,
            truncated: records.truncated,
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
pub struct Replayer<'a>(common::Replayer<'a>);

impl<'a> Replayer<'a> {
    pub fn new(recording: &'a Recording) -> Self {
        Self(common::Replayer::new(&recording.records))
    }

    /// Frames run so far.
    pub fn frame(&self) -> u64 {
        self.0.frame()
    }

    /// Apply the button changes due before the next frame, then run it.
    pub fn step_frame(&mut self, gb: &mut GameBoy) {
        self.0.step_frame(gb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_common::Button;

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
    fn every_game_boy_button_and_state_round_trips_and_l_r_are_refused() {
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
                let rec = Recording::decode(&bytes);
                if System::Gb.has_button(button) {
                    assert_eq!(rec.unwrap().records, vec![r]);
                } else {
                    assert!(matches!(
                        rec,
                        Err(ReplayError::Decode(DecodeError::BadRecord { index: 0, .. }))
                    ));
                }
            }
        }
    }

    /// Offset of the model (variant) byte in `sample()`'s header (3 battery bytes).
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

    /// Records shared by the hand-built files below: Start pressed at frame 0,
    /// a marker at 7, the end at 12.
    const RECORDS: [u8; 27] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0x87, //
        7, 0, 0, 0, 0, 0, 0, 0, 0x40, //
        12, 0, 0, 0, 0, 0, 0, 0, 0x41,
    ];

    fn hand_built(version: u8, tail: &[u8]) -> Vec<u8> {
        let mut v = b"GBINPUT\0".to_vec();
        v.push(version);
        v.extend_from_slice(&[0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01]);
        v.extend_from_slice(&[3, 0, 0, 0, 1, 2, 3]);
        v.extend_from_slice(tail);
        v.extend_from_slice(&RECORDS);
        v
    }

    fn check_hand_built(rec: &Recording, model: Model) {
        assert_eq!(rec.header.rom_hash, 0x0123_4567_89AB_CDEF);
        assert_eq!(rec.header.battery, Some(vec![1, 2, 3]));
        assert_eq!(rec.header.model, model);
        assert_eq!(rec.records.len(), 2);
        assert_eq!(rec.markers(), vec![7]);
        assert_eq!(rec.end_frame, Some(12));
    }

    #[test]
    fn version_1_decodes_as_dmg() {
        // No variant byte and no system byte after the battery.
        let rec = Recording::decode(&hand_built(1, &[])).unwrap();
        check_hand_built(&rec, Model::Dmg);
    }

    #[test]
    fn version_2_keeps_its_model() {
        for (id, model) in [(0, Model::Dmg), (1, Model::Cgb)] {
            let rec = Recording::decode(&hand_built(2, &[id])).unwrap();
            check_hand_built(&rec, model);
        }
        // The model byte is validated as before.
        assert_eq!(
            Recording::decode(&hand_built(2, &[2])),
            Err(ReplayError::BadModel(2))
        );
    }

    #[test]
    fn version_3_is_a_game_boy_with_system_byte_0() {
        let rec = Recording::decode(&hand_built(3, &[1, 0])).unwrap();
        check_hand_built(&rec, Model::Cgb);
        assert_eq!(
            Recording::decode(&hand_built(3, &[0, 1])),
            Err(ReplayError::WrongSystem(System::Gba))
        );
        assert_eq!(
            Recording::decode(&hand_built(3, &[0, 2])),
            Err(ReplayError::Decode(DecodeError::UnknownSystem(2)))
        );
    }

    /// A recording of another system is reported as that, whatever its battery
    /// size: a GBA save can be larger than any Game Boy cartridge's.
    #[test]
    fn another_systems_big_battery_is_wrong_system_not_battery_too_large() {
        for len in [128 * 1024, MAX_BATTERY_LEN + 1, 200 * 1024] {
            let bytes = common::encode_header(System::Gba, 1, Some(&vec![0; len]), 0);
            assert_eq!(
                Recording::decode(&bytes),
                Err(ReplayError::WrongSystem(System::Gba)),
                "{len} bytes"
            );
        }
        // The same size on a Game Boy recording is still refused, with its cap.
        let bytes = common::encode_header(System::Gb, 1, Some(&vec![0; 200 * 1024]), 0);
        assert_eq!(
            Recording::decode(&bytes),
            Err(ReplayError::Decode(DecodeError::BatteryTooLarge {
                len: 200 * 1024,
                max: MAX_BATTERY_LEN
            }))
        );
    }

    #[test]
    fn the_writer_emits_version_3_and_a_v2_reader_layout_still_matches() {
        let (header, records) = sample();
        let v = encode(&header, &records, Some(12));
        assert_eq!(v[8], 3);
        assert_eq!(v[MODEL_AT], Model::Cgb.id());
        assert_eq!(v[MODEL_AT + 1], 0, "system byte: Game Boy");
        assert_eq!(&v[17..21], &3u32.to_le_bytes(), "battery length offset");
    }

    #[test]
    fn rejects_an_unknown_model_byte() {
        let (header, records) = sample();
        let good = encode(&header, &records, Some(12));
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
        use DecodeError as D;
        let dec = |b: &[u8]| {
            Recording::decode(b).map_err(|e| match e {
                ReplayError::Decode(d) => d,
                other => panic!("{other:?}"),
            })
        };
        let (header, records) = sample();
        let good = encode(&header, &records, Some(12));
        assert_eq!(dec(b"nope"), Err(D::NotARecording));
        let mut v = good.clone();
        v[8] = VERSION + 1;
        assert_eq!(dec(&v), Err(D::UnsupportedVersion(VERSION + 1)));
        v[8] = 0;
        assert_eq!(dec(&v), Err(D::UnsupportedVersion(0)));
        assert_eq!(dec(&good[..15]), Err(D::TruncatedHeader));
        assert_eq!(dec(&good[..22]), Err(D::TruncatedHeader));
        // The battery is complete but the model byte, or the system byte, is missing.
        assert_eq!(dec(&good[..MODEL_AT]), Err(D::TruncatedHeader));
        assert_eq!(dec(&good[..MODEL_AT + 1]), Err(D::TruncatedHeader));

        let mut v = good.clone();
        v[17..21].copy_from_slice(&((MAX_BATTERY_LEN + 1) as u32).to_le_bytes());
        assert_eq!(
            dec(&v),
            Err(D::BatteryTooLarge {
                len: MAX_BATTERY_LEN + 1,
                max: MAX_BATTERY_LEN
            })
        );

        let first = encode_header(&header).len();
        let mut v = good.clone();
        v[first + 8] = 0x3F;
        assert_eq!(
            dec(&v),
            Err(D::BadRecord {
                index: 0,
                kind: 0x3F
            })
        );

        let mut v = encode(&header, &records, None);
        v.extend_from_slice(&encode_record(Record {
            frame: 1,
            event: Event::Marker,
        }));
        assert_eq!(dec(&v), Err(D::FramesOutOfOrder { index: 4 }));

        let mut v = good;
        v.extend_from_slice(&encode_record(Record {
            frame: 12,
            event: Event::Marker,
        }));
        assert_eq!(dec(&v), Err(D::RecordAfterEnd { index: 5 }));
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
        assert_eq!(player.0.pending(), 0, "the press was consumed");
    }
}
