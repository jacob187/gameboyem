//! Command-line parsing. Hand-rolled: the surface is tiny and ROM paths must
//! stay `OsString`-clean (no lossy UTF-8 round trip).

use std::ffi::OsString;
use std::path::PathBuf;

use gb_core::{CartridgeHeader, Model};

use crate::video::PaletteKind;

pub const USAGE: &str = "\
usage: gb [options] [--] <rom.gb|rom.gbc>

options:
  --scale N          integer window scale in logical points, 1..=8 (default 4)
  --mute             disable audio output
  --palette NAME     green (default) or grey (DMG only; colour games use their own colours)
  --model NAME       auto (default: Game Boy Color for CGB-flagged ROMs, else DMG), dmg or cgb
  --test-pattern     show a synthetic test pattern and tone instead of running the core
  --record FILE      record every input to FILE (must not exist) for `gb-test --replay`
  -h, --help         show this help
  --                 end of options; the next argument is the ROM path, even if it starts with `-`

controls:
  arrows or WASD = D-pad    X or E = A    Z = B    Enter = Start    Backspace / Right Shift = Select
  Q / R = GBA L / R shoulders (no effect on a Game Boy)
  P = pause    Tab (hold) = fast-forward    C = screenshot (marks the frame when recording)
  K = save state    L = load state    1-9 = state slot (default 1)
  Esc = quit";

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
    pub rom: PathBuf,
    pub scale: u32,
    pub mute: bool,
    pub palette: PaletteKind,
    pub model: ModelChoice,
    pub test_pattern: bool,
    pub record: Option<PathBuf>,
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
            "--scale" => {
                let value = option_value("--scale", args.next())?;
                scale = parse_scale(&value)?;
            }
            "--palette" => {
                let value = option_value("--palette", args.next())?;
                palette = parse_palette(&value)?;
            }
            "--model" => {
                let value = option_value("--model", args.next())?;
                model = parse_model(&value)?;
            }
            "--record" => record = Some(path_value("--record", args.next())?),
            _ if text.starts_with("--record=") && text.len() > "--record=".len() => {
                record = Some(PathBuf::from(&text["--record=".len()..]));
            }
            _ if text.starts_with("--scale=") => scale = parse_scale(&text["--scale=".len()..])?,
            _ if text.starts_with("--palette=") => {
                palette = parse_palette(&text["--palette=".len()..])?;
            }
            _ if text.starts_with("--model=") => model = parse_model(&text["--model=".len()..])?,
            _ if text.starts_with('-') && text.len() > 1 => {
                return Err(format!("unknown option `{text}`"));
            }
            _ => set_rom(&mut rom, arg)?,
        }
    }

    let rom = rom.ok_or("missing ROM path")?;
    if test_pattern && record.is_some() {
        return Err("--record cannot be combined with --test-pattern".into());
    }
    Ok(Command::Run(Options {
        rom,
        scale,
        mute,
        palette,
        model,
        test_pattern,
        record,
    }))
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
        assert_eq!(o.rom, PathBuf::from("game.gb"));
        assert_eq!(o.scale, 4);
        assert!(!o.mute && !o.test_pattern);
        assert_eq!(o.record, None);
        assert_eq!(o.palette, PaletteKind::Green);
        assert_eq!(o.model, ModelChoice::Auto);
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
    fn help() {
        assert_eq!(p(&["-h"]).unwrap(), Command::Help);
        assert_eq!(p(&["g.gb", "--help"]).unwrap(), Command::Help);
    }

    #[test]
    fn errors() {
        assert!(p(&[]).is_err());
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
        assert_eq!(run(&["--", "g.gb"]).rom, PathBuf::from("g.gb"));
        assert!(run(&["--mute", "--", "g.gb"]).mute);
        assert_eq!(run(&["--", "-weird.gb"]).rom, PathBuf::from("-weird.gb"));
        assert_eq!(run(&["--", "--mute"]).rom, PathBuf::from("--mute"));
        // Even `--help` and a second `--` are plain ROM paths after the first.
        assert_eq!(run(&["--", "--help"]).rom, PathBuf::from("--help"));
        assert_eq!(run(&["--", "--"]).rom, PathBuf::from("--"));
        // Options before the terminator still apply; none are parsed after it.
        let o = run(&["--scale", "2", "--", "-x.gb"]);
        assert_eq!((o.scale, o.rom), (2, PathBuf::from("-x.gb")));
        assert!(!run(&["--", "--mute"]).mute);
    }

    #[test]
    fn double_dash_still_needs_exactly_one_rom() {
        assert!(p(&["--"]).is_err());
        assert!(p(&["--", "a.gb", "b.gb"]).is_err());
        assert!(p(&["a.gb", "--", "b.gb"]).is_err());
        assert!(p(&["--mute"]).is_err());
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
        assert_eq!(o.rom.as_os_str(), raw.as_os_str());
    }
}
