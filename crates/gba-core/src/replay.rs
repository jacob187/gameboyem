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
//! A version 3 GBA recording (made before the extension existed) has none and
//! decodes as the defaults: `--save-type auto`, `--rtc auto`, the built-in BIOS
//! and no clock start. It replays as the frontend ran it then.
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
        let mut e = Vec::with_capacity(EXT_LEN_WITH_RTC);
        e.push(self.options.save.id());
        e.push(self.options.rtc.id());
        e.push(u8::from(self.bios_hash.is_some()));
        e.extend_from_slice(&self.bios_hash.unwrap_or(0).to_le_bytes());
        match self.rtc_start {
            None => e.push(0),
            Some(t) => {
                e.push(1);
                e.extend_from_slice(&t.year().to_le_bytes());
                e.extend_from_slice(&[t.month(), t.day(), t.hour(), t.minute(), t.second()]);
            }
        }
        e
    }
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
    /// exactly when the session used one.
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
