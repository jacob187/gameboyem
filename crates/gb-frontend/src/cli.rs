//! Command-line parsing. Hand-rolled: the surface is tiny and ROM paths must
//! stay `OsString`-clean (no lossy UTF-8 round trip).

use std::ffi::OsString;
use std::path::PathBuf;

use crate::video::PaletteKind;

pub const USAGE: &str = "\
usage: gb [options] <rom.gb>

options:
  --scale N          integer window scale in logical points, 1..=8 (default 4)
  --mute             disable audio output
  --palette NAME     green (default) or grey
  --test-pattern     show a synthetic test pattern and tone instead of running the core
  -h, --help         show this help

controls:
  arrows = D-pad    X = A    Z = B    Enter = Start    Backspace / Right Shift = Select
  P = pause    Tab (hold) = fast-forward    Esc = quit";

pub const DEFAULT_SCALE: u32 = 4;
pub const MAX_SCALE: u32 = 8;

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub rom: PathBuf,
    pub scale: u32,
    pub mute: bool,
    pub palette: PaletteKind,
    pub test_pattern: bool,
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
    let mut test_pattern = false;

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let Some(text) = arg.to_str() else {
            set_rom(&mut rom, arg)?;
            continue;
        };
        match text {
            "-h" | "--help" => return Ok(Command::Help),
            "--mute" => mute = true,
            "--test-pattern" => test_pattern = true,
            "--scale" => {
                let value = args.next().ok_or("--scale needs a value")?;
                scale = parse_scale(&value.to_string_lossy())?;
            }
            "--palette" => {
                let value = args.next().ok_or("--palette needs a value")?;
                palette = parse_palette(&value.to_string_lossy())?;
            }
            _ if text.starts_with("--scale=") => scale = parse_scale(&text["--scale=".len()..])?,
            _ if text.starts_with("--palette=") => {
                palette = parse_palette(&text["--palette=".len()..])?;
            }
            _ if text.starts_with('-') && text.len() > 1 => {
                return Err(format!("unknown option `{text}`"));
            }
            _ => set_rom(&mut rom, arg)?,
        }
    }

    let rom = rom.ok_or("missing ROM path")?;
    Ok(Command::Run(Options {
        rom,
        scale,
        mute,
        palette,
        test_pattern,
    }))
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
        assert_eq!(o.palette, PaletteKind::Green);
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
        assert!(p(&["--palette", "pink", "g.gb"]).is_err());
        assert!(p(&["--bogus", "g.gb"]).is_err());
        assert!(p(&["a.gb", "b.gb"]).is_err());
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
