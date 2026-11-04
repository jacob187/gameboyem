//! Headless speed benchmark:
//! `cargo run -p gb-test --release -- --bench [FRAMES] [REPEATS] [--check] [--min-speed N] [--print-pins]`.
//!
//! Runs a fixed number of frames (default 3600) of a few representative ROMs and
//! reports emulated frames/sec and the speed relative to real time (59.73 fps).
//! Nothing here opens a window or an audio device; audio is only drained and
//! hashed. Each ROM also prints an FNV-1a hash of every frame's framebuffer
//! plus the serial output, and a separate hash of the audio samples, so a
//! performance change can be checked for bit-identical behaviour by comparing
//! the hashes before and after. Not part of `cargo test`.
//!
//! The ROMs run on a DMG, except cgb-acid2, which runs on a CGB in colour mode
//! (`Model::Cgb`) to cover the colour renderer. Its video hash covers the RGB555
//! frame instead of the shade frame: every pixel as two little-endian bytes, in
//! row-major order. The serial and audio hashes are computed the same way for all
//! ROMs. A missing ROM fails the run, whichever ROM it is.
//!
//! The "overall" line is the mean speed across all ROMs (total frames over total
//! emulation time), so it covers the colour renderer as well as the DMG one;
//! `--min-speed` compares against it. It is not comparable with figures from
//! before cgb-acid2 joined the set (about 2650-2730 fps over the four DMG ROMs).
//! The per-ROM lines are unaffected.
//!
//! * `--check` compares each ROM against `crates/gb-test/bench-pins.txt` and fails
//!   if the emulation output differs from the pin, if no pin exists for this
//!   frame count, or if the ROM bytes differ from the pinned build (for example a
//!   homebrew source change; re-pin in that case).
//! * `--print-pins` prints pin lines for this run, to paste into the pins file.
//! * `--min-speed N` fails if the overall speed is below `N` times real time.
//!   CI uses a low floor because shared runners are slow and noisy.

use std::time::{Duration, Instant};

use gb_core::{FRAMES_PER_SECOND, GameBoy, Model};

use gb_test::{roms_dir, workspace_root};

/// ROMs (relative to the ROMs directory) in the benchmark, with the model each
/// one runs on.
const ROMS: [(&str, Model); 5] = [
    ("test/dmg-acid2/dmg-acid2.gb", Model::Dmg),
    ("test/blargg/cpu_instrs/cpu_instrs.gb", Model::Dmg),
    ("homebrew/snake.gb", Model::Dmg),
    ("homebrew/breakout.gb", Model::Dmg),
    ("test/cgb-acid2/cgb-acid2.gbc", Model::Cgb),
];

const DEFAULT_FRAMES: u32 = 3600;
const DEFAULT_REPEATS: u32 = 3;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(FNV_PRIME);
    }
    h
}

/// Fold an RGB555 frame into a hash as little-endian bytes, whatever the host.
fn fnv_frame16(mut h: u64, frame: &[u16]) -> u64 {
    for px in frame {
        h = fnv(h, &px.to_le_bytes());
    }
    h
}

struct Run {
    elapsed: Duration,
    video_hash: u64,
    audio_hash: u64,
}

fn run_once(rom: &[u8], model: Model, frames: u32) -> Result<Run, String> {
    let mut gb = GameBoy::with_model(rom.to_vec(), model).map_err(|e| e.to_string())?;
    let mut video_hash = FNV_OFFSET;
    let mut audio_hash = FNV_OFFSET;
    let mut elapsed = Duration::ZERO;
    for _ in 0..frames {
        let t = Instant::now();
        gb.step_frame();
        let audio = gb.audio_samples();
        elapsed += t.elapsed();
        video_hash = match model {
            Model::Dmg => fnv(video_hash, gb.framebuffer()),
            Model::Cgb => fnv_frame16(
                video_hash,
                gb.cgb_framebuffer()
                    .ok_or("no colour frame: the ROM did not start in CGB mode")?,
            ),
        };
        for s in &audio {
            audio_hash = fnv(audio_hash, &s.to_bits().to_le_bytes());
        }
    }
    video_hash = fnv(video_hash, gb.serial_output());
    Ok(Run {
        elapsed,
        video_hash,
        audio_hash,
    })
}

/// Pins file, relative to the workspace root.
const PINS_FILE: &str = "crates/gb-test/bench-pins.txt";

/// Expected output of one ROM for one frame count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pin {
    rom_hash: u64,
    video_hash: u64,
    audio_hash: u64,
}

/// Parse pin lines: `FRAMES ROM_HASH VIDEO_HASH AUDIO_HASH PATH` (hex hashes,
/// `#` comments). Malformed lines are errors so a typo can't silently disable
/// a check.
fn parse_pins(text: &str) -> Result<Vec<(u32, String, Pin)>, String> {
    let mut pins = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || format!("{PINS_FILE}:{}: malformed pin line", n + 1);
        let mut f = line.splitn(5, char::is_whitespace);
        let mut field = || f.next().ok_or_else(bad);
        let frames = field()?.parse().map_err(|_| bad())?;
        let mut hex =
            || -> Result<u64, String> { u64::from_str_radix(field()?, 16).map_err(|_| bad()) };
        let pin = Pin {
            rom_hash: hex()?,
            video_hash: hex()?,
            audio_hash: hex()?,
        };
        let path = field()?.trim().to_string();
        pins.push((frames, path, pin));
    }
    Ok(pins)
}

/// Why a ROM's run doesn't match its pin, or `None` if it does.
fn pin_mismatch(pin: Option<&Pin>, actual: &Pin) -> Option<&'static str> {
    match pin {
        None => Some("no pin for this frame count (add one with --print-pins)"),
        Some(p) if p.rom_hash != actual.rom_hash => {
            Some("ROM differs from the pinned build (rebuilt or edited?); re-pin with --print-pins")
        }
        Some(p) if (p.video_hash, p.audio_hash) != (actual.video_hash, actual.audio_hash) => {
            Some("emulation output differs from the pin")
        }
        Some(_) => None,
    }
}

/// Entry point for `--bench`. `args` are all command-line arguments.
pub fn run(args: &[String]) -> std::process::ExitCode {
    let check = args.iter().any(|a| a == "--check");
    let print_pins = args.iter().any(|a| a == "--print-pins");
    let mut min_speed: Option<f64> = None;
    let mut positional = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix("--min-speed=") {
            min_speed = v.parse().ok();
        } else if a == "--min-speed" {
            min_speed = it.next().and_then(|v| v.parse().ok());
        } else if !a.starts_with("--") {
            positional.push(a);
        }
    }
    if args.iter().any(|a| a.starts_with("--min-speed")) && min_speed.is_none() {
        eprintln!("--min-speed needs a number (times real time)");
        return std::process::ExitCode::FAILURE;
    }
    let parse = |i: usize, default: u32| {
        positional
            .get(i)
            .and_then(|a| a.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(default)
    };
    let frames = parse(0, DEFAULT_FRAMES);
    let repeats = parse(1, DEFAULT_REPEATS);
    let pins = if check {
        let path = workspace_root().join(PINS_FILE);
        match std::fs::read_to_string(&path)
            .map_err(|e| format!("{}: {e}", path.display()))
            .and_then(|t| parse_pins(&t))
        {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{e}");
                return std::process::ExitCode::FAILURE;
            }
        }
    } else {
        Vec::new()
    };
    let mut pin_lines = Vec::new();
    println!("{frames} frames per ROM, best of {repeats}; real time = {FRAMES_PER_SECOND:.2} fps");
    println!(
        "{:<38} {:>10} {:>10}  {:<17} {:<16}",
        "rom", "frames/s", "realtime", "video+serial hash", "audio hash"
    );
    let mut ok = true;
    let mut total = Duration::ZERO;
    for (name, model) in ROMS {
        let path = roms_dir().join(name);
        let Ok(rom) = std::fs::read(&path) else {
            println!("{name:<38} missing ({})", path.display());
            ok = false;
            continue;
        };
        let mut best: Option<Run> = None;
        for _ in 0..repeats {
            match run_once(&rom, model, frames) {
                Ok(r) => {
                    if let Some(b) = &best {
                        // Emulation is deterministic: every repeat must agree.
                        assert_eq!(
                            (b.video_hash, b.audio_hash),
                            (r.video_hash, r.audio_hash),
                            "{name}: nondeterministic output"
                        );
                    }
                    if best.as_ref().is_none_or(|b| r.elapsed < b.elapsed) {
                        best = Some(r);
                    }
                }
                Err(e) => {
                    println!("{name:<38} load error: {e}");
                    ok = false;
                    break;
                }
            }
        }
        let Some(r) = best else { continue };
        total += r.elapsed;
        let fps = f64::from(frames) / r.elapsed.as_secs_f64();
        let actual = Pin {
            rom_hash: fnv(FNV_OFFSET, &rom),
            video_hash: r.video_hash,
            audio_hash: r.audio_hash,
        };
        let verdict = if check {
            let pin = pins
                .iter()
                .find(|(f, p, _)| *f == frames && p == name)
                .map(|(_, _, pin)| pin);
            match pin_mismatch(pin, &actual) {
                Some(why) => {
                    ok = false;
                    format!("  MISMATCH: {why}")
                }
                None => "  pinned".to_string(),
            }
        } else {
            String::new()
        };
        println!(
            "{name:<38} {fps:>10.0} {:>9.1}x  {:016x}  {:016x}{verdict}",
            fps / FRAMES_PER_SECOND,
            r.video_hash,
            r.audio_hash
        );
        pin_lines.push(format!(
            "{frames} {:016x} {:016x} {:016x} {name}",
            actual.rom_hash, actual.video_hash, actual.audio_hash
        ));
    }
    let all = f64::from(frames) * ROMS.len() as f64 / total.as_secs_f64();
    let speed = all / FRAMES_PER_SECOND;
    println!("{:<38} {all:>10.0} {speed:>9.1}x", "overall");
    if let Some(floor) = min_speed {
        if speed < floor {
            println!("FAIL: overall {speed:.1}x is below the --min-speed floor of {floor}x");
            ok = false;
        } else {
            println!("speed floor {floor}x: ok");
        }
    }
    if print_pins {
        println!("# pins for {frames} frames:");
        for l in &pin_lines {
            println!("{l}");
        }
    }
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIN: Pin = Pin {
        rom_hash: 1,
        video_hash: 2,
        audio_hash: 3,
    };

    #[test]
    fn pins_parse_and_reject_malformed_lines() {
        let text =
            "# c\n\n1200 0000000000000001 0000000000000002 0000000000000003 homebrew/snake.gb\n";
        let pins = parse_pins(text).unwrap();
        assert_eq!(pins, vec![(1200, "homebrew/snake.gb".to_string(), PIN)]);
        assert!(parse_pins("1200 zz 2 3 a.gb").is_err());
        assert!(parse_pins("1200 1 2 3").is_err());
        assert!(parse_pins("x 1 2 3 a.gb").is_err());
    }

    #[test]
    fn mismatch_reasons_are_distinct() {
        assert_eq!(pin_mismatch(Some(&PIN), &PIN), None);
        assert!(pin_mismatch(None, &PIN).unwrap().contains("no pin"));
        let rom = Pin { rom_hash: 9, ..PIN };
        assert!(
            pin_mismatch(Some(&PIN), &rom)
                .unwrap()
                .contains("ROM differs")
        );
        let out = Pin {
            audio_hash: 9,
            ..PIN
        };
        assert!(
            pin_mismatch(Some(&PIN), &out)
                .unwrap()
                .contains("output differs")
        );
    }
}
