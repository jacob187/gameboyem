//! Input recordings for the GBA: everything needed to re-run a play session.
//!
//! The core is deterministic, so a session is fully described by the ROM, how
//! the machine was powered on and the ordered button changes keyed by frame. The
//! byte format (header framing, records, decoding) is shared with the other
//! cores and lives in [`emu_common::replay`]; this module adds what is GBA
//! specific: the header's system extension, the 10-button limit, the battery
//! size cap and powering a [`Gba`] on from a recording (DECISIONS #74).
//!
//! Extension layout (version 4 headers; all fields fixed width):
//!
//! ```text
//! save type u8 (SaveChoice::id) | clock option u8 (RtcMode::id)
//! | BIOS kind u8 (0 built-in HLE, 1 a real dump) | BIOS FNV-1a u64 (0 for HLE)
//! | clock start u8 (0 none, 1 given) | year u16 | month | day | hour | minute | second u8
//!                                       (the last six only when the clock start is given)
//! ```
//!
//! A version 3 GBA recording (made before the extension existed) has none. It
//! decodes as the defaults (`--save-type auto`, `--rtc auto`, the built-in BIOS,
//! no clock start) and is flagged [`Recording::legacy`]: it replays faithfully
//! only if the session used those defaults, and the file cannot say. Because it
//! does not say which BIOS it used either, [`Recording::power_on`] takes a BIOS
//! dump for it without being able to check it. Callers should warn when
//! `legacy` is set (deep-review #14).
//!
//! A real-BIOS recording stores the dump's hash, never the dump (it cannot be
//! shipped); replaying it needs the user's own dump, and says so otherwise.

use emu_common::System;
use emu_common::replay::{self as common, DecodeError};

use crate::cart::{CartError, CartOptions, RtcMode, RtcTime, SaveChoice};
use crate::{BIOS_SIZE, Gba};

pub use emu_common::replay::{Event, MAGIC, RECORD_LEN, Record, VERSION, encode_record, rom_hash};

/// Largest battery image any supported cartridge produces: 128 KiB of Flash
/// plus the 24-byte RTC footer.
pub const MAX_BATTERY_LEN: usize = 128 * 1024 + crate::cart::RTC_FOOTER_LEN;

/// Extension length without / with a clock start.
const EXT_LEN: usize = 12;
const EXT_LEN_WITH_RTC: usize = EXT_LEN + 7;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub rom_hash: u64,
    /// `Gba::battery_save()` right after the session's save was loaded.
    pub battery: Option<Vec<u8>>,
    /// `--save-type` and `--rtc`.
    pub options: CartOptions,
    /// FNV-1a of the real BIOS dump the session booted through; `None` for the
    /// built-in HLE BIOS.
    pub bios_hash: Option<u64>,
    /// `--rtc-start`, applied before the battery was loaded.
    pub rtc_start: Option<RtcTime>,
}

impl Header {
    /// The header of a session on `gba`, as it stands right after the save was
    /// loaded (before any frame ran).
    pub fn of(gba: &Gba) -> Self {
        Self {
            rom_hash: gba.rom_hash(),
            battery: gba.battery_save(),
            options: gba.options(),
            bios_hash: gba.bios_hash(),
            rtc_start: gba.rtc_start(),
        }
    }

    /// The extension bytes (what `Core::recording_ext` returns).
    pub fn ext(&self) -> Vec<u8> {
        encode_ext(self.options, self.bios_hash, self.rtc_start)
    }
}

/// The header extension for a session started with these options, BIOS and clock
/// start (see the module docs for the layout).
pub fn encode_ext(
    options: CartOptions,
    bios_hash: Option<u64>,
    rtc_start: Option<RtcTime>,
) -> Vec<u8> {
    let mut e = Vec::with_capacity(EXT_LEN_WITH_RTC);
    e.push(options.save.id());
    e.push(options.rtc.id());
    e.push(u8::from(bios_hash.is_some()));
    e.extend_from_slice(&bios_hash.unwrap_or(0).to_le_bytes());
    match rtc_start {
        None => e.push(0),
        Some(t) => {
            e.push(1);
            e.extend_from_slice(&t.year().to_le_bytes());
            e.extend_from_slice(&[t.month(), t.day(), t.hour(), t.minute(), t.second()]);
        }
    }
    e
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
    /// A version 3 file: no extension, so `header` holds the defaults rather
    /// than what the session used (see the module docs).
    pub legacy: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplayError {
    /// The shared format rejected the file (bad magic, version, record, ...).
    Decode(DecodeError),
    /// A well-formed recording of a console this core does not emulate.
    WrongSystem(System),
    /// The hardware-variant byte is not 0, the GBA's only variant.
    BadVariant(u8),
    /// The header's extension is malformed.
    BadExtension(&'static str),
    WrongRom {
        expected: u64,
        actual: u64,
    },
    /// The session booted through a real BIOS dump and none was given.
    NeedsBios {
        hash: u64,
    },
    /// A BIOS was given for a session that used the built-in one.
    UnexpectedBios,
    /// The BIOS file is not 16 KiB.
    BadBiosSize(usize),
    /// The BIOS file is not the dump the session used.
    WrongBios {
        expected: u64,
        actual: u64,
    },
    Cartridge(CartError),
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
            Self::WrongSystem(s) => write!(f, "recording is for a {s:?} machine, not a GBA"),
            Self::BadVariant(v) => {
                write!(f, "recording has an unknown hardware variant byte {v:#04X}")
            }
            Self::BadExtension(why) => write!(f, "recording header is invalid ({why})"),
            Self::WrongRom { expected, actual } => write!(
                f,
                "recording was made with a different ROM (hash {expected:016x}, this ROM {actual:016x})"
            ),
            Self::NeedsBios { hash } => write!(
                f,
                "recording was made with a real BIOS (hash {hash:016x}); replaying it needs the same dump (--bios FILE)"
            ),
            Self::UnexpectedBios => write!(
                f,
                "recording was made with the built-in BIOS; replaying it with a real BIOS would diverge (drop --bios)"
            ),
            Self::BadBiosSize(n) => {
                write!(f, "the BIOS file is {n} bytes, expected {BIOS_SIZE}")
            }
            Self::WrongBios { expected, actual } => write!(
                f,
                "recording was made with a different BIOS (hash {expected:016x}, this file {actual:016x})"
            ),
            Self::Cartridge(e) => write!(f, "cannot start the recording: {e}"),
        }
    }
}

impl std::error::Error for ReplayError {}

/// The bytes of `header` (version 4: the extension is never empty).
pub fn encode_header(header: &Header) -> Vec<u8> {
    common::encode_header_ext(
        System::Gba,
        header.rom_hash,
        header.battery.as_deref(),
        0,
        &header.ext(),
    )
}

/// Parse the extension of a decoded header. An empty one is a version 3 file:
/// the defaults.
fn decode_ext(ext: &[u8]) -> Result<(CartOptions, Option<u64>, Option<RtcTime>), ReplayError> {
    if ext.is_empty() {
        return Ok((CartOptions::default(), None, None));
    }
    let bad = ReplayError::BadExtension;
    let rtc_given = match (ext.len(), ext.get(EXT_LEN - 1)) {
        (EXT_LEN, Some(0)) => false,
        (EXT_LEN_WITH_RTC, Some(1)) => true,
        _ => return Err(bad("wrong length or clock flag")),
    };
    let save = SaveChoice::from_id(ext[0]).ok_or(bad("save type"))?;
    let rtc = RtcMode::from_id(ext[1]).ok_or(bad("clock option"))?;
    let hash = u64::from_le_bytes(ext[3..11].try_into().expect("8 bytes"));
    let bios_hash = match ext[2] {
        0 if hash == 0 => None,
        1 => Some(hash),
        _ => return Err(bad("BIOS kind")),
    };
    let rtc_start = if rtc_given {
        let year = u16::from_le_bytes([ext[12], ext[13]]);
        let t = RtcTime::new(year, ext[14], ext[15], ext[16], ext[17], ext[18])
            .map_err(|_| bad("clock start"))?;
        Some(t)
    } else {
        None
    };
    Ok((CartOptions { save, rtc }, bios_hash, rtc_start))
}

impl Recording {
    /// Parse a recording. The input is untrusted: every length and value is checked.
    pub fn decode(bytes: &[u8]) -> Result<Self, ReplayError> {
        let (header, rest) = common::decode_header(bytes)?;
        if header.system != System::Gba {
            return Err(ReplayError::WrongSystem(header.system));
        }
        common::check_battery(&header, MAX_BATTERY_LEN)?;
        if header.variant != 0 {
            return Err(ReplayError::BadVariant(header.variant));
        }
        let (options, bios_hash, rtc_start) = decode_ext(&header.ext)?;
        let records = common::decode_records(rest, System::Gba.button_count())?;
        Ok(Self {
            header: Header {
                rom_hash: header.rom_hash,
                battery: header.battery,
                options,
                bios_hash,
                rtc_start,
            },
            records: records.records,
            end_frame: records.end_frame,
            truncated: records.truncated,
            legacy: header.ext.is_empty(),
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

    /// Power on a machine in the state the recorded session started from: the
    /// same cartridge options, BIOS, clock start and battery image, applied in
    /// the order the frontend applied them. `bios` is the user's dump, needed
    /// exactly when the session used one. A [`legacy`](Recording::legacy)
    /// recording does not say, so a dump is accepted for it unchecked.
    pub fn power_on(&self, rom: Vec<u8>, bios: Option<Box<[u8]>>) -> Result<Gba, ReplayError> {
        let actual = rom_hash(&rom);
        if actual != self.header.rom_hash {
            return Err(ReplayError::WrongRom {
                expected: self.header.rom_hash,
                actual,
            });
        }
        match (self.header.bios_hash, &bios) {
            (None, None) => {}
            (None, Some(data)) if self.legacy => {
                if data.len() != BIOS_SIZE {
                    return Err(ReplayError::BadBiosSize(data.len()));
                }
            }
            (None, Some(_)) => return Err(ReplayError::UnexpectedBios),
            (Some(hash), None) => return Err(ReplayError::NeedsBios { hash }),
            (Some(expected), Some(data)) => {
                if data.len() != BIOS_SIZE {
                    return Err(ReplayError::BadBiosSize(data.len()));
                }
                let actual = rom_hash(data);
                if actual != expected {
                    return Err(ReplayError::WrongBios { expected, actual });
                }
            }
        }
        let mut gba =
            Gba::with_options(rom, bios, self.header.options).map_err(ReplayError::Cartridge)?;
        if let Some(start) = self.header.rtc_start {
            gba.set_rtc(start);
        }
        if let Some(battery) = &self.header.battery {
            gba.load_battery_ram(battery)
                .map_err(ReplayError::Cartridge)?;
        }
        Ok(gba)
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
    pub fn step_frame(&mut self, gba: &mut Gba) {
        self.0.step_frame(gba);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emu_common::Button;

    fn rom(id: &[u8]) -> Vec<u8> {
        let mut rom = vec![0u8; 0x400];
        rom[..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes()); // b .
        rom[0x100..0x100 + id.len()].copy_from_slice(id);
        rom
    }

    fn bios(fill: u8) -> Box<[u8]> {
        vec![fill; BIOS_SIZE].into_boxed_slice()
    }

    fn start(button: Button, pressed: bool, frame: u64) -> Record {
        Record {
            frame,
            event: Event::Button { button, pressed },
        }
    }

    fn bytes(header: &Header, records: &[Record]) -> Vec<u8> {
        let mut out = encode_header(header);
        for r in records {
            out.extend_from_slice(&encode_record(*r));
        }
        out
    }

    fn header(rom: &[u8]) -> Header {
        Header {
            rom_hash: rom_hash(rom),
            battery: None,
            options: CartOptions::default(),
            bios_hash: None,
            rtc_start: None,
        }
    }

    #[test]
    fn the_header_extension_round_trips_every_option() {
        let t = RtcTime::new(2024, 2, 29, 13, 45, 9).unwrap();
        for save in [
            SaveChoice::Auto,
            SaveChoice::None,
            SaveChoice::Sram,
            SaveChoice::Flash64,
            SaveChoice::Flash128,
            SaveChoice::Eeprom512,
            SaveChoice::Eeprom8k,
        ] {
            for rtc in [RtcMode::Auto, RtcMode::On, RtcMode::Off] {
                for bios_hash in [None, Some(0xDEAD_BEEF_0123_4567)] {
                    for rtc_start in [None, Some(t)] {
                        for battery in [None, Some(vec![1, 2, 3])] {
                            let h = Header {
                                rom_hash: 77,
                                battery,
                                options: CartOptions { save, rtc },
                                bios_hash,
                                rtc_start,
                            };
                            let records = [start(Button::L, true, 0), start(Button::L, false, 4)];
                            let rec = Recording::decode(&bytes(&h, &records)).unwrap();
                            assert_eq!(rec.header, h);
                            assert_eq!(rec.records, records);
                            assert_eq!(h.ext().len(), if rtc_start.is_some() { 19 } else { 12 });
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_version_3_gba_recording_decodes_as_the_defaults() {
        let rom = rom(b"FLASH1M_V");
        // Exactly what the frontend wrote before the extension existed.
        let v3 = emu_common::replay::encode_header(System::Gba, rom_hash(&rom), None, 0);
        assert_eq!(v3[8], 3);
        let rec = Recording::decode(&v3).unwrap();
        assert_eq!(
            rec.header,
            Header {
                battery: None,
                ..header(&rom)
            }
        );
        let gba = rec.power_on(rom, None).unwrap();
        assert_eq!(gba.options(), CartOptions::default());
        assert_eq!(gba.bios_hash(), None);
    }

    /// A version 3 file cannot say which BIOS or options its session used: it is
    /// flagged so a caller can warn, and (unlike a version 4 file of a built-in
    /// BIOS session) it takes a BIOS dump, unchecked, so a session made with
    /// `--bios` can still be replayed (deep-review #14).
    #[test]
    fn a_version_3_recording_is_flagged_and_takes_a_bios_unchecked() {
        let rom = rom(b"");
        let v3 = emu_common::replay::encode_header(System::Gba, rom_hash(&rom), None, 0);
        let legacy = Recording::decode(&v3).unwrap();
        assert!(legacy.legacy);
        let gba = legacy.power_on(rom.clone(), Some(bios(7))).unwrap();
        assert_eq!(gba.bios_hash(), Some(rom_hash(&bios(7))));
        assert_eq!(
            legacy
                .power_on(rom.clone(), Some(vec![0; 100].into_boxed_slice()))
                .err(),
            Some(ReplayError::BadBiosSize(100))
        );
        // A version 4 file says what it used, and is held to it.
        let current = Recording::decode(&bytes(&header(&rom), &[])).unwrap();
        assert!(!current.legacy);
        assert_eq!(
            current.power_on(rom, Some(bios(7))).err(),
            Some(ReplayError::UnexpectedBios)
        );
    }

    #[test]
    fn a_session_header_is_what_powers_the_replay_on() {
        // A session with a forced save type, the clock on and started at a time.
        let rom = rom(b"");
        let options = CartOptions {
            save: SaveChoice::Flash128,
            rtc: RtcMode::On,
        };
        let t = RtcTime::new(2031, 12, 31, 23, 59, 58).unwrap();
        let mut live = Gba::with_options(rom.clone(), Some(bios(7)), options).unwrap();
        live.set_rtc(t);
        let h = Header::of(&live);
        assert_eq!(h.options, options);
        assert_eq!(h.bios_hash, Some(rom_hash(&bios(7))));
        assert_eq!(h.rtc_start, Some(t));
        assert_eq!(h.battery.as_ref().map(Vec::len), Some(128 * 1024));
        let rec = Recording::decode(&bytes(&h, &[])).unwrap();
        let replayed = rec.power_on(rom, Some(bios(7))).unwrap();
        assert_eq!(replayed.options(), options);
        assert_eq!(replayed.rtc_time(), live.rtc_time());
        assert_eq!(replayed.battery_save(), live.battery_save());
        assert_eq!(replayed.save_state(), live.save_state());
    }

    #[test]
    fn the_battery_loads_after_the_clock_start_as_the_frontend_does() {
        // A save with a footer replaces the clock start, as `--rtc-start` then
        // the `.sav` load do in the frontend.
        let rom = rom(b"FLASH1M_V");
        let mut first = Gba::new(rom.clone(), None).unwrap();
        first.set_rtc(RtcTime::new(2020, 1, 2, 3, 4, 5).unwrap());
        let footer_save = {
            let mut g = Gba::new(rom.clone(), None).unwrap();
            g.set_rtc(RtcTime::new(2030, 6, 7, 8, 9, 10).unwrap());
            let mut image = g.battery_ram().unwrap().to_vec();
            image.extend_from_slice(&footer_of(&g));
            image
        };
        first.load_battery_ram(&footer_save).unwrap();
        let h = Header::of(&first);
        let rec = Recording::decode(&bytes(&h, &[])).unwrap();
        let replayed = rec.power_on(rom, None).unwrap();
        assert_eq!(replayed.rtc_time(), first.rtc_time());
        assert_eq!(replayed.rtc_time().year(), 2030, "the save's clock won");
    }

    /// A valid RTC footer for `g`'s clock: the save image with the footer
    /// appended, minus the chip.
    fn footer_of(g: &Gba) -> Vec<u8> {
        let t = g.rtc_time();
        let bcd = |v: u8| ((v / 10) << 4) | (v % 10);
        let mut f = b"AGBRTC01".to_vec();
        f.extend_from_slice(&[
            0x40,
            bcd((t.year() - 2000) as u8),
            bcd(t.month()),
            bcd(t.day()),
            t.weekday(),
            bcd(t.hour()),
            bcd(t.minute()),
            bcd(t.second()),
        ]);
        f.extend_from_slice(&[0; 8]);
        f
    }

    #[test]
    fn power_on_says_what_is_wrong() {
        let rom = rom(b"");
        let mut h = header(&rom);
        let rec = |h: &Header| Recording::decode(&bytes(h, &[])).unwrap();
        // Another ROM.
        let mut other = rom.clone();
        other[0x300] = 1;
        assert!(matches!(
            rec(&h).power_on(other, None),
            Err(ReplayError::WrongRom { .. })
        ));
        // Built-in BIOS recording, BIOS supplied.
        assert_eq!(
            rec(&h).power_on(rom.clone(), Some(bios(1))).err(),
            Some(ReplayError::UnexpectedBios)
        );
        // Real BIOS recording: none, the wrong size, the wrong dump, the right one.
        h.bios_hash = Some(rom_hash(&bios(1)));
        let r = rec(&h);
        assert_eq!(
            r.power_on(rom.clone(), None).err(),
            Some(ReplayError::NeedsBios {
                hash: rom_hash(&bios(1))
            })
        );
        assert_eq!(
            r.power_on(rom.clone(), Some(vec![0; 100].into_boxed_slice()))
                .err(),
            Some(ReplayError::BadBiosSize(100))
        );
        assert!(matches!(
            r.power_on(rom.clone(), Some(bios(2))),
            Err(ReplayError::WrongBios { .. })
        ));
        assert!(r.power_on(rom, Some(bios(1))).is_ok());
        for e in [
            ReplayError::NeedsBios { hash: 1 },
            ReplayError::UnexpectedBios,
            ReplayError::BadBiosSize(3),
            ReplayError::WrongBios {
                expected: 1,
                actual: 2,
            },
        ] {
            assert!(!e.to_string().is_empty());
        }
        assert!(
            ReplayError::NeedsBios { hash: 1 }
                .to_string()
                .contains("--bios")
        );
    }

    #[test]
    fn a_damaged_extension_is_refused() {
        let rom = rom(b"");
        let good = emu_common::replay::encode_header_ext(
            System::Gba,
            rom_hash(&rom),
            None,
            0,
            &header(&rom).ext(),
        );
        assert!(Recording::decode(&good).is_ok());
        let ext_at = good.len() - EXT_LEN;
        let bad = |edit: &dyn Fn(&mut Vec<u8>)| {
            let mut v = good.clone();
            edit(&mut v);
            Recording::decode(&v)
        };
        for (what, at, value) in [
            ("save type", 0, 99),
            ("clock option", 1, 99),
            ("BIOS kind", 2, 5),
            ("a BIOS hash on the built-in BIOS", 3, 1),
            ("clock flag", 11, 2),
        ] {
            assert!(
                matches!(
                    bad(&|v| v[ext_at + at] = value),
                    Err(ReplayError::BadExtension(_))
                ),
                "{what}"
            );
        }
        // A clock start flag without the time, and a time that is not a date.
        let t = RtcTime::new(2024, 2, 29, 0, 0, 0).unwrap();
        let with_time = Header {
            rtc_start: Some(t),
            ..header(&rom)
        };
        let good_time = bytes(&with_time, &[]);
        assert!(Recording::decode(&good_time).is_ok());
        let mut v = good_time.clone();
        let n = v.len();
        v[n - 4] = 31; // day 31 of month 2
        assert!(matches!(
            Recording::decode(&v),
            Err(ReplayError::BadExtension(_))
        ));
        let mut v = good.clone();
        v[ext_at + 11] = 1; // flag set, no time follows
        assert!(matches!(
            Recording::decode(&v),
            Err(ReplayError::BadExtension(_))
        ));
        // A variant other than 0, and an extension that is not ours at all.
        let mut v = good.clone();
        v[good.len() - EXT_LEN - 3] = 1; // the variant byte
        assert_eq!(Recording::decode(&v), Err(ReplayError::BadVariant(1)));
        let short =
            emu_common::replay::encode_header_ext(System::Gba, rom_hash(&rom), None, 0, &[1, 2, 3]);
        assert!(matches!(
            Recording::decode(&short),
            Err(ReplayError::BadExtension(_))
        ));
        // Cut anywhere in the header: always an error, never a panic.
        for cut in 0..good.len() {
            assert!(Recording::decode(&good[..cut]).is_err(), "cut {cut}");
        }
    }

    #[test]
    fn other_systems_and_the_battery_cap() {
        let gb = emu_common::replay::encode_header(System::Gb, 1, None, 0);
        assert_eq!(
            Recording::decode(&gb),
            Err(ReplayError::WrongSystem(System::Gb))
        );
        let big =
            emu_common::replay::encode_header(System::Gba, 1, Some(&vec![0; MAX_BATTERY_LEN]), 0);
        assert!(Recording::decode(&big).is_ok());
        let bigger = emu_common::replay::encode_header(
            System::Gba,
            1,
            Some(&vec![0; MAX_BATTERY_LEN + 1]),
            0,
        );
        assert!(matches!(
            Recording::decode(&bigger),
            Err(ReplayError::Decode(DecodeError::BatteryTooLarge { .. }))
        ));
    }

    #[test]
    fn a_replay_feeds_the_recorded_buttons_to_the_machine() {
        let rom = rom(b"");
        let records = [
            start(Button::L, true, 1),
            start(Button::A, true, 2),
            Record {
                frame: 2,
                event: Event::Marker,
            },
            start(Button::L, false, 3),
            Record {
                frame: 4,
                event: Event::End,
            },
        ];
        let rec = Recording::decode(&bytes(&header(&rom), &records)).unwrap();
        assert_eq!((rec.markers(), rec.frame_count()), (vec![2], 4));
        let mut gba = rec.power_on(rom, None).unwrap();
        let mut player = Replayer::new(&rec);
        let mut keys = Vec::new();
        while player.frame() < rec.frame_count() {
            player.step_frame(&mut gba);
            keys.push(gba.peek16(0x0400_0130) & 0x3FF);
        }
        // Active low: L is bit 9, A bit 0. A change recorded at frame f takes
        // effect before the (f+1)-th frame.
        assert_eq!(
            keys,
            [0x3FF, 0x3FF & !0x200, 0x3FF & !0x201, 0x3FF & !0x001]
        );
    }
}
