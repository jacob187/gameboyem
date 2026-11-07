//! Command-line parsing. Hand-rolled: the surface is tiny and ROM paths must
//! stay `OsString`-clean (no lossy UTF-8 round trip).

use std::ffi::OsString;
use std::path::PathBuf;

use emu_common::System;
use gb_core::{CartridgeHeader, Model};
use gba_core::{RtcMode, RtcTime, SaveChoice};

use crate::video::PaletteKind;

pub const USAGE: &str = "\
usage: gb [options] [--] [rom.gb|rom.gbc|rom.gba]

With no ROM path, a file dialog asks for one. A ROM file can also be dropped onto the window.
Saves, the resume point, snapshots and screenshots are kept in the app data folder, not next to the
ROM; a .sav next to the ROM is copied there the first time the ROM is opened, and left in place
(old numbered save states there become snapshots).

Saving: the game's own save is kept automatically. When you leave a game, the app remembers exactly
where you were and continues there next time (not with --record or --test-pattern). Snapshots save
the game anywhere (K or the pause menu) and load again later (L, or the menu for any of them).

options:
  --scale N          integer window scale in logical points, 1..=8 (default 4)
  --mute             disable audio output
  --system NAME      gb or gba (default: a .gba file, or a ROM with a GBA header, is a GBA game)
  --palette NAME     green (default) or grey (Game Boy only; colour games use their own colours)
  --model NAME       auto (default: Game Boy Color for CGB-flagged ROMs, else DMG), dmg or cgb (Game Boy only)
  --bios FILE        boot through a real 16 KiB GBA BIOS dump (GBA only; default: built-in HLE BIOS)
  --save-type NAME   auto (default: from the ROM), none, sram, flash64, flash128, eeprom512 or eeprom8k (GBA only)
  --rtc NAME         auto (default: on when the game sets up the cartridge clock), on or off (GBA only)
  --rtc-start TIME   start the GBA cartridge clock at YYYY-MM-DDTHH:MM:SS if the save has none (GBA only; default 2000-01-01, power-loss flag set)
  --add              copy the ROM into the game library (in the app data folder) and exit; opens no window
  --test-pattern     show a synthetic test pattern and tone instead of running the core
  --record FILE      record every input to FILE (must not exist) for `gb-test --replay`
  -h, --help         show this help
  --                 end of options; the next argument is the ROM path, even if it starts with `-`

environment:
  GAMEBOYEM_DATA_DIR use this folder for saves, snapshots, screenshots, the library and logs
                     (default: macOS ~/Library/Application Support/gameboyem,
                     Windows %APPDATA%\\gameboyem\\data, Linux ~/.local/share/gameboyem)
  GAMEBOYEM_LOG      log level in logs/gameboyem.log: error, warn, info (default), debug or trace

controls:
  arrows or WASD = D-pad    X or E = A    Z = B    Enter = Start    Backspace / Right Shift = Select
  Q / R = GBA L / R shoulders (no effect on a Game Boy)
  P = pause    Tab (hold) = fast-forward    C = screenshot (marks the frame when recording)
  K = save snapshot    L = load the latest snapshot
  (S used to save a state; WASD now drives the D-pad, so S is D-pad Down)
  Esc = pause menu (snapshots, restart, back to library); quit from the library";

pub const DEFAULT_SCALE: u32 = 4;
pub const MAX_SCALE: u32 = 8;

/// `--model`: which console to emulate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelChoice {
    /// Follow the cartridge header ([`Model::from_header`]).
    Auto,
    Dmg,
    Cgb,
}

impl ModelChoice {
    /// The model to power on for `rom`. An unparsable header falls back to a
    /// DMG; the core then reports the real cartridge error on construction.
    pub fn resolve(self, rom: &[u8]) -> Model {
        match self {
            ModelChoice::Dmg => Model::Dmg,
            ModelChoice::Cgb => Model::Cgb,
            ModelChoice::Auto => {
                CartridgeHeader::parse(rom).map_or(Model::Dmg, |h| Model::from_header(&h))
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    /// The ROM to run. `None` when none was given; see [`crate::picker::startup`].
    pub rom: Option<PathBuf>,
    pub scale: u32,
    pub mute: bool,
    pub palette: PaletteKind,
    pub model: ModelChoice,
    pub test_pattern: bool,
    pub record: Option<PathBuf>,
    /// `--add`: import the ROM into the library and exit.
    pub add: bool,
    /// `--system`: overrides the choice made from the file name and header.
    pub system: Option<System>,
    /// `--bios`: a GBA BIOS dump.
    pub bios: Option<PathBuf>,
    /// `--save-type`: the GBA backup chip (default: detect from the ROM).
    pub save_type: SaveChoice,
    /// `--rtc`: whether the GBA cartridge has a clock.
    pub rtc: RtcMode,
    /// `--rtc-start`: the GBA cartridge clock's starting time. The only way real
    /// time reaches the core: the user states it (the core reads no host
    /// clock), so a run is reproducible.
    pub rtc_start: Option<RtcTime>,
    /// GBA-only flags the user passed, so a Game Boy run can say they are ignored.
    pub gba_only: Vec<&'static str>,
    /// Game Boy-only flags the user passed (a flag equal to its default still
    /// counts), so a GBA run can say they are ignored; see [`check_system_flags`].
    pub gb_only: Vec<&'static str>,
}

impl Options {
    /// The options the player passed that change how a game powers on or what a state of it
    /// holds (`--model`, `--system`, `--bios`, `--save-type`, `--rtc`, `--rtc-start`), in the
    /// order of that list. A resume point was made without them, so it is not used when any
    /// is given. `--palette` only changes the colours drawn, so it is not one.
    pub fn power_on_flags(&self) -> Vec<&'static str> {
        let passed = |flag: &str| self.gb_only.contains(&flag) || self.gba_only.contains(&flag);
        let mut flags = Vec::new();
        for (flag, given) in [
            ("--model", passed("--model")),
            ("--system", self.system.is_some()),
            ("--bios", self.bios.is_some()),
            ("--save-type", passed("--save-type")),
            ("--rtc", passed("--rtc")),
            ("--rtc-start", passed("--rtc-start")),
        ] {
            if given {
                flags.push(flag);
            }
        }
        flags
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(Options),
    Help,
}

/// Parse arguments (without the program name).
pub fn parse<I>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut rom: Option<PathBuf> = None;
    let mut scale = DEFAULT_SCALE;
    let mut mute = false;
    let mut palette = PaletteKind::Green;
    let mut model = ModelChoice::Auto;
    let mut test_pattern = false;
    let mut record: Option<PathBuf> = None;
    let mut add = false;
    let mut system: Option<System> = None;
    let mut bios: Option<PathBuf> = None;
    let mut save_type = SaveChoice::Auto;
    let mut rtc = RtcMode::Auto;
    let mut rtc_start: Option<RtcTime> = None;
    let mut gba_only: Vec<&'static str> = Vec::new();
    let mut gb_only: Vec<&'static str> = Vec::new();

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let Some(text) = arg.to_str() else {
            set_rom(&mut rom, arg)?;
            continue;
        };
        match text {
            "--" => {
                // Everything after the terminator is a ROM path, whatever it looks like.
                for rest in args.by_ref() {
                    set_rom(&mut rom, rest)?;
                }
            }
            "-h" | "--help" => return Ok(Command::Help),
            "--mute" => mute = true,
            "--test-pattern" => test_pattern = true,
            "--add" => add = true,
            "--scale" => {
                let value = option_value("--scale", args.next())?;
                scale = parse_scale(&value)?;
            }
            "--palette" => {
                let value = option_value("--palette", args.next())?;
                palette = parse_palette(&value)?;
                push_unique(&mut gb_only, "--palette");
            }
            "--model" => {
                let value = option_value("--model", args.next())?;
                model = parse_model(&value)?;
                push_unique(&mut gb_only, "--model");
            }
            "--system" => {
                let value = option_value("--system", args.next())?;
                system = Some(parse_system(&value)?);
            }
            "--save-type" => {
                let value = option_value("--save-type", args.next())?;
                save_type = parse_save_type(&value)?;
                push_unique(&mut gba_only, "--save-type");
            }
            "--rtc" => {
                let value = option_value("--rtc", args.next())?;
                rtc = parse_rtc(&value)?;
                push_unique(&mut gba_only, "--rtc");
            }
            "--rtc-start" => {
                let value = option_value("--rtc-start", args.next())?;
                rtc_start = Some(parse_rtc_start(&value)?);
                push_unique(&mut gba_only, "--rtc-start");
            }
            _ if text.starts_with("--save-type=") => {
                save_type = parse_save_type(&text["--save-type=".len()..])?;
                push_unique(&mut gba_only, "--save-type");
            }
            _ if text.starts_with("--rtc=") => {
                rtc = parse_rtc(&text["--rtc=".len()..])?;
                push_unique(&mut gba_only, "--rtc");
            }
            _ if text.starts_with("--rtc-start=") => {
                rtc_start = Some(parse_rtc_start(&text["--rtc-start=".len()..])?);
                push_unique(&mut gba_only, "--rtc-start");
            }
            "--bios" => bios = Some(path_value("--bios", args.next())?),
            _ if text.starts_with("--bios=") && text.len() > "--bios=".len() => {
                bios = Some(PathBuf::from(&text["--bios=".len()..]));
            }
            _ if text.starts_with("--system=") => {
                system = Some(parse_system(&text["--system=".len()..])?);
            }
            "--record" => record = Some(path_value("--record", args.next())?),
            _ if text.starts_with("--record=") && text.len() > "--record=".len() => {
                record = Some(PathBuf::from(&text["--record=".len()..]));
            }
            _ if text.starts_with("--scale=") => scale = parse_scale(&text["--scale=".len()..])?,
            _ if text.starts_with("--palette=") => {
                palette = parse_palette(&text["--palette=".len()..])?;
                push_unique(&mut gb_only, "--palette");
            }
            _ if text.starts_with("--model=") => {
                model = parse_model(&text["--model=".len()..])?;
                push_unique(&mut gb_only, "--model");
            }
            // Older macOS passes `-psn_0_<n>` when Finder opens an app; it is not a ROM or an option.
            _ if text.starts_with("-psn_") => {}
            _ if text.starts_with('-') && text.len() > 1 => {
                return Err(format!("unknown option `{text}`"));
            }
            _ => set_rom(&mut rom, arg)?,
        }
    }

    if test_pattern && record.is_some() {
        return Err("--record cannot be combined with --test-pattern".into());
    }
    if add && rom.is_none() {
        return Err("--add needs a ROM path".into());
    }
    if add && (record.is_some() || test_pattern) {
        return Err("--add cannot be combined with --record or --test-pattern".into());
    }
    Ok(Command::Run(Options {
        rom,
        scale,
        mute,
        palette,
        model,
        test_pattern,
        record,
        add,
        system,
        bios,
        save_type,
        rtc,
        rtc_start,
        gba_only,
        gb_only,
    }))
}

/// Record that `flag` was passed, once however often it was (the warning names
/// each flag one time).
fn push_unique(flags: &mut Vec<&'static str>, flag: &'static str) {
    if !flags.contains(&flag) {
        flags.push(flag);
    }
}

/// Check the options that depend on the system, once it is known. `--bios` on a
/// Game Boy game is an error (a user mistake with no safe meaning). `--model`
/// and `--palette` on a GBA game, and `--save-type`, `--rtc` and `--rtc-start`
/// on a Game Boy game, are harmless, so a launcher that always passes them keeps
/// working: the warning to print (`Ok(Some(..))`) says they are ignored.
pub fn check_system_flags(system: System, o: &Options) -> Result<Option<String>, String> {
    match system {
        System::Gba if !o.gb_only.is_empty() => {
            Ok(Some(format!("{} ignored for GBA", o.gb_only.join(" and "))))
        }
        System::Gba => Ok(None),
        System::Gb if o.bios.is_some() => Err(
            "--bios only applies to GBA games (use --system gba to run this as a GBA ROM)"
                .to_owned(),
        ),
        System::Gb if !o.gba_only.is_empty() => Ok(Some(format!(
            "{} ignored for Game Boy games",
            o.gba_only.join(" and ")
        ))),
        System::Gb => Ok(None),
    }
}

/// Like [`option_value`] for a file path: kept as an `OsString` (no lossy UTF-8).
fn path_value(name: &str, value: Option<OsString>) -> Result<PathBuf, String> {
    match value {
        Some(v) if !(v.to_string_lossy().starts_with('-') && v.len() > 1) => Ok(PathBuf::from(v)),
        Some(v) => Err(format!(
            "{name} is missing a value (found option `{}`)",
            v.to_string_lossy()
        )),
        None => Err(format!("{name} is missing a value")),
    }
}

/// The argument after `name`, which must exist and must not look like another
/// option (`--scale --mute` means the value is missing, not that it is `--mute`).
/// A bare `-` is not option-like.
fn option_value(name: &str, value: Option<OsString>) -> Result<String, String> {
    match value {
        Some(v) => {
            let v = v.to_string_lossy();
            if v.starts_with('-') && v.len() > 1 {
                Err(format!("{name} is missing a value (found option `{v}`)"))
            } else {
                Ok(v.into_owned())
            }
        }
        None => Err(format!("{name} is missing a value")),
    }
}

fn set_rom(slot: &mut Option<PathBuf>, arg: OsString) -> Result<(), String> {
    if slot.is_some() {
        return Err("more than one ROM path given".into());
    }
    *slot = Some(PathBuf::from(arg));
    Ok(())
}

fn parse_scale(s: &str) -> Result<u32, String> {
    match s.parse::<u32>() {
        Ok(n) if (1..=MAX_SCALE).contains(&n) => Ok(n),
        _ => Err(format!(
            "--scale must be an integer 1..={MAX_SCALE}, got `{s}`"
        )),
    }
}

fn parse_palette(s: &str) -> Result<PaletteKind, String> {
    match s {
        "green" => Ok(PaletteKind::Green),
        "grey" | "gray" => Ok(PaletteKind::Grey),
        _ => Err(format!("--palette must be `green` or `grey`, got `{s}`")),
    }
}

fn parse_system(s: &str) -> Result<System, String> {
    match s {
        "gb" => Ok(System::Gb),
        "gba" => Ok(System::Gba),
        _ => Err(format!("--system must be `gb` or `gba`, got `{s}`")),
    }
}

fn parse_save_type(s: &str) -> Result<SaveChoice, String> {
    match s {
        "auto" => Ok(SaveChoice::Auto),
        "none" => Ok(SaveChoice::None),
        "sram" => Ok(SaveChoice::Sram),
        "flash64" => Ok(SaveChoice::Flash64),
        "flash128" => Ok(SaveChoice::Flash128),
        "eeprom512" => Ok(SaveChoice::Eeprom512),
        "eeprom8k" => Ok(SaveChoice::Eeprom8k),
        _ => Err(format!(
            "--save-type must be `auto`, `none`, `sram`, `flash64`, `flash128`, `eeprom512` or `eeprom8k`, got `{s}`"
        )),
    }
}

fn parse_rtc(s: &str) -> Result<RtcMode, String> {
    match s {
        "auto" => Ok(RtcMode::Auto),
        "on" => Ok(RtcMode::On),
        "off" => Ok(RtcMode::Off),
        _ => Err(format!("--rtc must be `auto`, `on` or `off`, got `{s}`")),
    }
}

/// `YYYY-MM-DDTHH:MM:SS`, digits only, a real date in 2000..=2099.
fn parse_rtc_start(s: &str) -> Result<RtcTime, String> {
    let bad = || {
        format!(
            "--rtc-start must be a date and time like 2024-02-29T13:45:00 (2000..2099), got `{s}`"
        )
    };
    let b = s.as_bytes();
    let shape_ok = b.len() == 19
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && b.iter()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7 | 10 | 13 | 16) || c.is_ascii_digit());
    if !shape_ok {
        return Err(bad());
    }
    let n = |r: std::ops::Range<usize>| s[r].parse::<u16>().map_err(|_| bad());
    RtcTime::new(
        n(0..4)?,
        n(5..7)? as u8,
        n(8..10)? as u8,
        n(11..13)? as u8,
        n(14..16)? as u8,
        n(17..19)? as u8,
    )
    .map_err(|_| bad())
}

fn parse_model(s: &str) -> Result<ModelChoice, String> {
    match s {
        "auto" => Ok(ModelChoice::Auto),
        "dmg" => Ok(ModelChoice::Dmg),
        "cgb" => Ok(ModelChoice::Cgb),
        _ => Err(format!("--model must be `auto`, `dmg` or `cgb`, got `{s}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Command, String> {
        parse(args.iter().map(OsString::from))
    }

    fn run(args: &[&str]) -> Options {
        match p(args).unwrap() {
            Command::Run(o) => o,
            Command::Help => panic!("unexpected help"),
        }
    }

    #[test]
    fn defaults() {
        let o = run(&["game.gb"]);
        assert_eq!(o.rom, Some(PathBuf::from("game.gb")));
        assert_eq!(o.scale, 4);
        assert!(!o.mute && !o.test_pattern);
        assert_eq!(o.record, None);
        assert!(!o.add);
        assert_eq!(o.palette, PaletteKind::Green);
        assert_eq!(o.model, ModelChoice::Auto);
        assert_eq!((o.system, o.bios.as_deref()), (None, None));
        assert!(o.gb_only.is_empty());
    }

    #[test]
    fn add_imports_a_rom_and_needs_one() {
        let o = run(&["--add", "game.gb"]);
        assert!(o.add);
        assert_eq!(o.rom, Some(PathBuf::from("game.gb")));
        assert!(p(&["--add"]).unwrap_err().contains("needs a ROM path"));
        assert!(p(&["--add", "--record", "r.gbinput", "g.gb"]).is_err());
        assert!(p(&["--add", "--test-pattern", "g.gb"]).is_err());
    }

    #[test]
    fn system_flag() {
        assert_eq!(run(&["--system", "gba", "g.gb"]).system, Some(System::Gba));
        assert_eq!(run(&["g.gba", "--system=gb"]).system, Some(System::Gb));
        assert!(p(&["--system", "gbc", "g.gb"]).is_err());
        assert!(p(&["--system=", "g.gb"]).is_err());
        assert!(
            p(&["--system", "--mute", "g.gb"])
                .unwrap_err()
                .contains("missing a value")
        );
    }

    #[test]
    fn bios_flag() {
        assert_eq!(
            run(&["--bios", "b.bin", "g.gba"]).bios,
            Some(PathBuf::from("b.bin"))
        );
        assert_eq!(
            run(&["g.gba", "--bios=b.bin"]).bios,
            Some(PathBuf::from("b.bin"))
        );
        assert!(
            p(&["g.gba", "--bios"])
                .unwrap_err()
                .contains("missing a value")
        );
        assert!(p(&["--bios=", "g.gba"]).is_err());
    }

    #[test]
    fn save_type_flag() {
        for (name, want) in [
            ("auto", SaveChoice::Auto),
            ("none", SaveChoice::None),
            ("sram", SaveChoice::Sram),
            ("flash64", SaveChoice::Flash64),
            ("flash128", SaveChoice::Flash128),
            ("eeprom512", SaveChoice::Eeprom512),
            ("eeprom8k", SaveChoice::Eeprom8k),
        ] {
            assert_eq!(run(&["--save-type", name, "g.gba"]).save_type, want);
            assert_eq!(
                run(&["g.gba", &format!("--save-type={name}")]).save_type,
                want
            );
        }
        assert_eq!(run(&["g.gba"]).save_type, SaveChoice::Auto);
        for bad in ["flash", "FLASH64", "eeprom", "", "sram32"] {
            let e = p(&["--save-type", bad, "g.gba"]).unwrap_err();
            assert!(e.contains("--save-type must be"), "{bad}: {e}");
        }
        assert!(p(&["--save-type=", "g.gba"]).is_err());
        assert!(
            p(&["g.gba", "--save-type"])
                .unwrap_err()
                .contains("missing a value")
        );
        assert!(
            p(&["--save-type", "--mute", "g.gba"])
                .unwrap_err()
                .contains("missing a value")
        );
    }

    #[test]
    fn rtc_flags() {
        assert_eq!(run(&["g.gba"]).rtc, RtcMode::Auto);
        assert_eq!(run(&["g.gba"]).rtc_start, None);
        for (name, want) in [
            ("auto", RtcMode::Auto),
            ("on", RtcMode::On),
            ("off", RtcMode::Off),
        ] {
            assert_eq!(run(&["--rtc", name, "g.gba"]).rtc, want);
            assert_eq!(run(&[&format!("--rtc={name}"), "g.gba"]).rtc, want);
        }
        assert!(
            p(&["--rtc", "yes", "g.gba"])
                .unwrap_err()
                .contains("--rtc must be")
        );
        assert!(p(&["--rtc=", "g.gba"]).is_err());
        assert!(
            p(&["g.gba", "--rtc"])
                .unwrap_err()
                .contains("missing a value")
        );

        let t = run(&["--rtc-start", "2024-02-29T13:45:09", "g.gba"])
            .rtc_start
            .unwrap();
        assert_eq!(
            (
                t.year(),
                t.month(),
                t.day(),
                t.weekday(),
                t.hour(),
                t.minute(),
                t.second()
            ),
            (2024, 2, 29, 4, 13, 45, 9)
        );
        assert_eq!(
            run(&["--rtc-start=2000-01-01T00:00:00", "g.gba"])
                .rtc_start
                .unwrap()
                .weekday(),
            6
        );
        for bad in [
            "2024-02-30T00:00:00",
            "2023-02-29T00:00:00",
            "1999-12-31T23:59:59",
            "2100-01-01T00:00:00",
            "2024-13-01T00:00:00",
            "2024-01-01T24:00:00",
            "2024-01-01 00:00:00",
            "2024-01-01T00:00",
            "2024-1-1T0:0:0",
            "2024-01-01T00:00:0\u{e9}",
            "+024-01-01T00:00:00",
            "now",
            "",
        ] {
            let e = p(&["--rtc-start", bad, "g.gba"]).unwrap_err();
            assert!(e.contains("--rtc-start must be"), "{bad}: {e}");
        }
    }

    #[test]
    fn gba_only_flags_are_ignored_with_a_warning_for_a_game_boy() {
        for (args, flags) in [
            (&["--save-type", "sram", "g.gb"][..], "--save-type"),
            (&["--rtc=off", "g.gb"], "--rtc"),
            (&["--rtc-start=2024-01-01T00:00:00", "g.gb"], "--rtc-start"),
            (
                &["--save-type=none", "--rtc", "on", "g.gb"],
                "--save-type and --rtc",
            ),
        ] {
            let o = run(args);
            assert_eq!(
                check_system_flags(System::Gb, &o),
                Ok(Some(format!("{flags} ignored for Game Boy games"))),
                "{args:?}"
            );
            assert_eq!(check_system_flags(System::Gba, &o), Ok(None));
        }
        // A flag repeated on the command line is named once.
        let o = run(&[
            "--rtc",
            "on",
            "--save-type=sram",
            "--rtc=off",
            "--rtc",
            "auto",
            "g.gb",
        ]);
        assert_eq!(
            check_system_flags(System::Gb, &o),
            Ok(Some(
                "--rtc and --save-type ignored for Game Boy games".to_owned()
            ))
        );
        let o = run(&[
            "--model",
            "dmg",
            "--model=cgb",
            "--palette=grey",
            "--palette",
            "green",
            "g.gba",
        ]);
        assert_eq!(
            check_system_flags(System::Gba, &o),
            Ok(Some("--model and --palette ignored for GBA".to_owned()))
        );
        // --bios on a Game Boy game stays an error, even next to a GBA-only flag.
        let o = run(&["--bios", "b.bin", "--rtc", "on", "g.gb"]);
        assert!(check_system_flags(System::Gb, &o).is_err());
    }

    #[test]
    fn game_boy_only_flags_are_ignored_with_a_warning_for_a_gba() {
        for (args, flags) in [
            (&["--model", "auto", "g.gba"][..], "--model"),
            (&["--model=dmg", "g.gba"], "--model"),
            (&["--palette", "green", "g.gba"], "--palette"),
            (&["--palette=grey", "g.gba"], "--palette"),
            (
                &["--model=cgb", "--palette=grey", "g.gba"],
                "--model and --palette",
            ),
        ] {
            let o = run(args);
            assert_eq!(
                check_system_flags(System::Gba, &o),
                Ok(Some(format!("{flags} ignored for GBA"))),
                "{args:?}"
            );
            assert_eq!(check_system_flags(System::Gb, &o), Ok(None));
        }
        let o = run(&["--scale", "2", "--mute", "g.gba"]);
        assert_eq!(check_system_flags(System::Gba, &o), Ok(None));
    }

    #[test]
    fn bios_is_refused_for_a_game_boy() {
        let o = run(&["--bios", "b.bin", "g.gb"]);
        let e = check_system_flags(System::Gb, &o).unwrap_err();
        assert!(e.contains("--bios only applies to GBA"), "{e}");
        assert_eq!(check_system_flags(System::Gba, &o), Ok(None));
    }

    #[test]
    fn model_flag() {
        assert_eq!(run(&["--model", "dmg", "g.gb"]).model, ModelChoice::Dmg);
        assert_eq!(run(&["g.gbc", "--model=cgb"]).model, ModelChoice::Cgb);
        assert_eq!(run(&["--model=auto", "g.gb"]).model, ModelChoice::Auto);
        assert!(p(&["--model", "gba", "g.gb"]).is_err());
        assert!(p(&["--model=", "g.gb"]).is_err());
        assert!(p(&["g.gb", "--model"]).is_err());
        assert!(
            p(&["--model", "--mute", "g.gb"])
                .unwrap_err()
                .contains("missing a value")
        );
    }

    #[test]
    fn model_resolves_from_the_header_only_in_auto() {
        let mut rom = vec![0u8; 0x8000];
        assert_eq!(ModelChoice::Auto.resolve(&rom), Model::Dmg);
        for flag in [0x80, 0xC0] {
            rom[0x143] = flag;
            assert_eq!(ModelChoice::Auto.resolve(&rom), Model::Cgb);
        }
        assert_eq!(ModelChoice::Dmg.resolve(&rom), Model::Dmg);
        rom[0x143] = 0;
        assert_eq!(ModelChoice::Cgb.resolve(&rom), Model::Cgb);
        // A ROM too small for a header falls back to the DMG (the core rejects it).
        assert_eq!(ModelChoice::Auto.resolve(&[0xC0; 4]), Model::Dmg);
    }

    #[test]
    fn flags_in_any_order() {
        let o = run(&[
            "--mute",
            "g.gb",
            "--scale",
            "2",
            "--palette=grey",
            "--test-pattern",
        ]);
        assert_eq!((o.scale, o.mute, o.test_pattern), (2, true, true));
        assert_eq!(o.palette, PaletteKind::Grey);
        assert_eq!(run(&["--scale=3", "g.gb"]).scale, 3);
    }

    #[test]
    fn record_path() {
        assert_eq!(
            run(&["--record", "s.gbinput", "g.gb"]).record,
            Some(PathBuf::from("s.gbinput"))
        );
        assert_eq!(
            run(&["g.gb", "--record=s.gbinput"]).record,
            Some(PathBuf::from("s.gbinput"))
        );
        assert!(
            p(&["g.gb", "--record"])
                .unwrap_err()
                .contains("missing a value")
        );
        assert!(
            p(&["--record", "--mute", "g.gb"])
                .unwrap_err()
                .contains("missing a value")
        );
        assert!(p(&["--record=", "g.gb"]).is_err());
        assert!(p(&["--record", "s", "--test-pattern", "g.gb"]).is_err());
    }

    #[test]
    fn power_on_options_are_the_ones_that_change_a_state() {
        let flags = |args: &[&str]| run(args).power_on_flags();
        assert!(flags(&["g.gb"]).is_empty());
        assert!(flags(&["--scale", "2", "--mute", "--palette", "grey", "g.gb"]).is_empty());
        assert_eq!(flags(&["--model", "dmg", "g.gb"]), ["--model"]);
        assert_eq!(
            flags(&["--model=auto", "g.gb"]),
            ["--model"],
            "even the default value"
        );
        assert_eq!(
            flags(&[
                "--rtc-start=2001-02-03T04:05:06",
                "--bios",
                "b.bin",
                "--save-type",
                "sram",
                "--rtc",
                "on",
                "--system",
                "gba",
                "g.gba"
            ]),
            ["--system", "--bios", "--save-type", "--rtc", "--rtc-start"]
        );
    }

    #[test]
    fn help() {
        assert_eq!(p(&["-h"]).unwrap(), Command::Help);
        assert_eq!(p(&["g.gb", "--help"]).unwrap(), Command::Help);
    }

    #[test]
    fn help_says_where_save_state_went() {
        assert!(USAGE.contains("K = save snapshot    L = load the latest snapshot"));
        assert!(!USAGE.contains("1-9"), "no slot numbers");
        assert!(!USAGE.to_lowercase().contains("slot"));
        assert!(USAGE.contains("S used to save a state"));
        assert!(USAGE.contains("S is D-pad Down"));
    }

    #[test]
    fn errors() {
        // No arguments is valid: the file dialog asks for a ROM.
        assert!(matches!(p(&[]), Ok(Command::Run(o)) if o.rom.is_none()));
        assert!(p(&["--scale", "0", "g.gb"]).is_err());
        assert!(p(&["--scale", "9", "g.gb"]).is_err());
        assert!(p(&["--scale", "x", "g.gb"]).is_err());
        assert!(p(&["--scale"]).is_err());
        assert!(p(&["g.gb", "--palette"]).is_err());
        assert!(p(&["--palette", "pink", "g.gb"]).is_err());
        assert!(p(&["--bogus", "g.gb"]).is_err());
        assert!(p(&["a.gb", "b.gb"]).is_err());
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(run(&["--", "g.gb"]).rom, Some(PathBuf::from("g.gb")));
        assert!(run(&["--mute", "--", "g.gb"]).mute);
        assert_eq!(
            run(&["--", "-weird.gb"]).rom,
            Some(PathBuf::from("-weird.gb"))
        );
        assert_eq!(run(&["--", "--mute"]).rom, Some(PathBuf::from("--mute")));
        // Even `--help` and a second `--` are plain ROM paths after the first.
        assert_eq!(run(&["--", "--help"]).rom, Some(PathBuf::from("--help")));
        assert_eq!(run(&["--", "--"]).rom, Some(PathBuf::from("--")));
        // Options before the terminator still apply; none are parsed after it.
        let o = run(&["--scale", "2", "--", "-x.gb"]);
        assert_eq!((o.scale, o.rom), (2, Some(PathBuf::from("-x.gb"))));
        assert!(!run(&["--", "--mute"]).mute);
    }

    #[test]
    fn double_dash_allows_at_most_one_rom() {
        assert!(p(&["--", "a.gb", "b.gb"]).is_err());
        assert!(p(&["a.gb", "--", "b.gb"]).is_err());
    }

    #[test]
    fn the_process_serial_number_argument_from_finder_is_ignored() {
        assert_eq!(run(&["-psn_0_12345"]).rom, None);
        assert_eq!(
            run(&["-psn_0_12345", "g.gb"]).rom,
            Some(PathBuf::from("g.gb"))
        );
        // After `--` it is a ROM path like any other.
        assert_eq!(
            run(&["--", "-psn_0_1"]).rom,
            Some(PathBuf::from("-psn_0_1"))
        );
        assert!(p(&["-pxn_0_1"]).is_err());
    }

    #[test]
    fn the_rom_is_optional_so_a_launcher_can_ask_for_one() {
        assert_eq!(run(&[]).rom, None);
        assert_eq!(run(&["--"]).rom, None);
        let o = run(&["--mute", "--scale", "2"]);
        assert_eq!((o.rom, o.mute, o.scale), (None, true, 2));
    }

    #[test]
    fn option_value_must_not_look_like_a_flag() {
        for args in [
            &["--scale", "--mute", "g.gb"][..],
            &["--scale", "-2", "g.gb"],
            &["--palette", "--", "g.gb"],
            &["--palette", "-h", "g.gb"],
            &["g.gb", "--scale", "--test-pattern"],
        ] {
            let e = p(args).unwrap_err();
            assert!(e.contains("missing a value"), "{args:?}: {e}");
        }
        assert!(p(&["--scale"]).unwrap_err().contains("missing a value"));
        // `--opt=value` is unambiguous, so a dashed value is just invalid.
        assert!(
            p(&["--scale=-2", "g.gb"])
                .unwrap_err()
                .contains("--scale must be")
        );
        // A bare `-` is not option-like; it is rejected as a value, not as "missing".
        assert!(
            p(&["--scale", "-", "g.gb"])
                .unwrap_err()
                .contains("--scale must be")
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_rom_path_is_kept() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(vec![b'r', 0xFF, b'.', b'g', b'b']);
        let Command::Run(o) = parse([raw.clone()]).unwrap() else {
            panic!()
        };
        assert_eq!(
            o.rom.as_deref().map(|r| r.as_os_str()),
            Some(raw.as_os_str())
        );
    }
}
