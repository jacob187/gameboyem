//! `gb-test --replay REC ROM [--bios FILE] [--dump F,F,...] [--out DIR]`: play
//! back a recording made with `gb --record` headlessly and dump frames as PNGs.
//!
//! By default the dumped frames are every marker (screenshot) frame plus the
//! last frame. `--dump` replaces that with an explicit list. The system comes
//! from the recording. A Game Boy is powered on as the recorded model (version 1
//! files are DMG); a GBA with the recorded cartridge options, clock start and
//! BIOS: the built-in one, or the user's own dump (`--bios FILE`), which a
//! recording made with a real BIOS needs and says so without. The PNGs
//! (greyscale shades, or RGB for a CGB in colour mode and for a GBA) match the
//! frontend's screenshots byte for byte.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use gb_core::replay::{Recording, ReplayError, Replayer};
use gb_core::rgb555_to_rgb888;
use gb_test::gba::{gba_rgb, save_gba_rgb_png};
use gb_test::{artifact_dir, save_framebuffer_png, save_rgb_png};

const USAGE: &str = "usage: gb-test --replay REC ROM [--bios FILE] [--dump F,F,...] [--out DIR]";

pub fn run(args: &[String]) -> ExitCode {
    match run_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

struct Options {
    recording: PathBuf,
    rom: PathBuf,
    /// `--bios`: a GBA recording's real BIOS dump.
    bios: Option<PathBuf>,
    dump: Option<Vec<u64>>,
    out: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut positional = Vec::new();
    let mut dump = None;
    let mut bios = None;
    let mut out = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--replay" => {}
            "--dump" => {
                let v = it.next().ok_or("--dump is missing a value")?;
                dump = Some(parse_frames(v)?);
            }
            "--bios" => bios = Some(PathBuf::from(it.next().ok_or("--bios is missing a value")?)),
            "--out" => out = Some(PathBuf::from(it.next().ok_or("--out is missing a value")?)),
            s if s.starts_with("--") => return Err(format!("unknown option `{s}`")),
            _ => positional.push(PathBuf::from(a)),
        }
    }
    let [recording, rom]: [PathBuf; 2] = positional
        .try_into()
        .map_err(|_| "expected a recording and a ROM path".to_owned())?;
    Ok(Options {
        recording,
        rom,
        bios,
        dump,
        out,
    })
}

fn parse_frames(s: &str) -> Result<Vec<u64>, String> {
    s.split(',')
        .map(|f| {
            f.trim()
                .parse::<u64>()
                .map_err(|_| format!("--dump expects frame numbers like 100,250; got `{f}`"))
        })
        .collect()
}

/// Upper bound on frames a replay will run (~10 hours of play), so a corrupt
/// or hostile file cannot make the tool spin forever.
const MAX_FRAMES: u64 = 10 * 3600 * 60;

fn run_inner(args: &[String]) -> Result<(), String> {
    let opts = parse(args)?;
    let bytes = std::fs::read(&opts.recording)
        .map_err(|e| format!("cannot read {}: {e}", opts.recording.display()))?;
    let rom =
        std::fs::read(&opts.rom).map_err(|e| format!("cannot read {}: {e}", opts.rom.display()))?;
    // The Game Boy decoder names a recording of another system; that one is the
    // GBA's to decode (and to report any damage in).
    match Recording::decode(&bytes) {
        Ok(rec) => {
            if opts.bios.is_some() {
                return Err("--bios is for GBA recordings".to_owned());
            }
            replay_gb(&opts, &rec, rom)
        }
        Err(ReplayError::WrongSystem(_)) => {
            let rec = gba_core::replay::Recording::decode(&bytes).map_err(|e| e.to_string())?;
            replay_gba(&opts, &rec, rom)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// The frames to dump, validated against [`MAX_FRAMES`]: every marker and the
/// end unless `--dump` lists them.
fn targets(
    opts: &Options,
    markers: Vec<u64>,
    total: u64,
    truncated: bool,
    has_end: bool,
) -> Result<BTreeSet<u64>, String> {
    if !has_end {
        eprintln!(
            "warning: recording has no end record (session cut short); replaying to its last input"
        );
    }
    if truncated {
        eprintln!("warning: recording ends inside a record; the partial record was ignored");
    }
    let targets: BTreeSet<u64> = match &opts.dump {
        Some(list) => list.iter().copied().collect(),
        None => markers.into_iter().chain([total]).collect(),
    };
    let last = targets.last().copied().unwrap_or(total).max(total);
    if last > MAX_FRAMES {
        return Err(format!("refusing to run {last} frames (max {MAX_FRAMES})"));
    }
    Ok(targets)
}

fn output_dir(opts: &Options) -> Result<PathBuf, String> {
    let out = opts
        .out
        .clone()
        .unwrap_or_else(|| default_out(&opts.recording));
    std::fs::create_dir_all(&out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    Ok(out)
}

fn replay_gb(opts: &Options, rec: &Recording, rom: Vec<u8>) -> Result<(), String> {
    let mut gb = rec.power_on(rom).map_err(|e| e.to_string())?;

    let total = rec.frame_count();
    let targets = targets(
        opts,
        rec.markers(),
        total,
        rec.truncated,
        rec.end_frame.is_some(),
    )?;
    let out = output_dir(opts)?;

    println!(
        "replaying {} ({:?}, {total} frames, {} inputs, markers {:?})",
        opts.recording.display(),
        rec.header.model,
        rec.records.len() - rec.markers().len(),
        rec.markers()
    );
    let mut player = Replayer::new(rec);
    let mut audio = Vec::new();
    for &target in &targets {
        while player.frame() < target {
            player.step_frame(&mut gb);
            audio.clear();
            gb.drain_audio_into(&mut audio); // keep the core's buffer bounded
        }
        let path = out.join(format!("f{target:07}.png"));
        let hash = match gb.cgb_framebuffer() {
            // A CGB in colour mode: the RGB555 frame, widened without correction.
            Some(fb) => {
                let rgb: Vec<[u8; 3]> = fb.iter().map(|&c| rgb555_to_rgb888(c)).collect();
                save_rgb_png(&rgb, &path)?;
                frame_hash(&rgb.concat())
            }
            None => {
                save_framebuffer_png(gb.framebuffer(), &path)?;
                frame_hash(gb.framebuffer())
            }
        };
        println!("frame {target}: {} (hash {hash:016x})", path.display());
    }
    Ok(())
}

/// Read a GBA BIOS dump, refusing anything but exactly 16 KiB before reading it.
fn read_bios(path: &Path) -> Result<Box<[u8]>, String> {
    let len = std::fs::metadata(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?
        .len();
    if len != gba_core::BIOS_SIZE as u64 {
        return Err(format!(
            "{} is {len} bytes, a GBA BIOS is {}",
            path.display(),
            gba_core::BIOS_SIZE
        ));
    }
    std::fs::read(path)
        .map(Vec::into_boxed_slice)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))
}

fn replay_gba(
    opts: &Options,
    rec: &gba_core::replay::Recording,
    rom: Vec<u8>,
) -> Result<(), String> {
    let bios = opts.bios.as_deref().map(read_bios).transpose()?;
    let mut gba = rec.power_on(rom, bios).map_err(|e| e.to_string())?;

    let total = rec.frame_count();
    let targets = targets(
        opts,
        rec.markers(),
        total,
        rec.truncated,
        rec.end_frame.is_some(),
    )?;
    let out = output_dir(opts)?;

    let header = &rec.header;
    println!(
        "replaying {} (GBA, {} BIOS, save {:?}, rtc {:?}, {total} frames, {} inputs, markers {:?})",
        opts.recording.display(),
        if header.bios_hash.is_some() {
            "real"
        } else {
            "built-in"
        },
        header.options.save,
        header.options.rtc,
        rec.records.len() - rec.markers().len(),
        rec.markers()
    );
    let mut player = gba_core::replay::Replayer::new(rec);
    let mut audio = Vec::new();
    for &target in &targets {
        while player.frame() < target {
            player.step_frame(&mut gba);
            audio.clear();
            gba.drain_audio_into(&mut audio); // keep the core's buffer bounded
        }
        let path = out.join(format!("f{target:07}.png"));
        let rgb = gba_rgb(gba.framebuffer());
        save_gba_rgb_png(&rgb, &path)?;
        println!(
            "frame {target}: {} (hash {:016x})",
            path.display(),
            frame_hash(&rgb)
        );
    }
    Ok(())
}

fn default_out(recording: &Path) -> PathBuf {
    let stem = recording
        .file_stem()
        .map_or_else(|| "replay".into(), |s| s.to_string_lossy().into_owned());
    artifact_dir().join("replay").join(stem)
}

fn frame_hash(fb: &[u8]) -> u64 {
    gb_core::replay::rom_hash(fb)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_options() {
        let o = parse(&args(&[
            "--replay",
            "r.gbinput",
            "g.gb",
            "--dump",
            "1, 20",
            "--out",
            "d",
            "--bios",
            "b.bin",
        ]))
        .unwrap();
        assert_eq!(o.bios, Some(PathBuf::from("b.bin")));
        assert_eq!(o.recording, PathBuf::from("r.gbinput"));
        assert_eq!(o.rom, PathBuf::from("g.gb"));
        assert_eq!(o.dump, Some(vec![1, 20]));
        assert_eq!(o.out, Some(PathBuf::from("d")));
        assert!(parse(&args(&["--replay", "r"])).is_err());
        assert!(parse(&args(&["--replay", "r", "g", "x"])).is_err());
        assert!(parse(&args(&["--replay", "r", "g", "--dump", "a"])).is_err());
        assert!(parse(&args(&["--replay", "r", "g", "--bogus"])).is_err());
        assert!(parse(&args(&["--replay", "r", "g", "--bios"])).is_err());
    }
}
