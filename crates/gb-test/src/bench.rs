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
//! Four GBA ROMs run alongside: FuzzARM `ARM_Any` (CPU), the `moth` and
//! `grains` homebrew (mode 0 with direct-sound audio; mode 4) and tonc's `m7_demo` (mode 7
//! affine). Their video hash covers each frame's 240x160 RGB555 buffer (little-endian
//! halfwords) and there is no serial output; the audio hash is over the host-rate f32
//! samples at the default 48 kHz. `moth` and `grains` have START held for frames 15..19 so
//! they leave the title screen and play (the title screen would exercise little); the input
//! is part of the pinned output.
//!
//! The ROMs run on a DMG, except cgb-acid2 and uCity, which run on a CGB in colour
//! mode (`Model::Cgb`): cgb-acid2 covers the colour renderer, uCity switches to
//! double speed at about frame 15 and so covers that clocking path. Their video
//! hashes cover the RGB555 frame instead of the shade frame: every pixel as two little-endian bytes, in
//! row-major order. The serial and audio hashes are computed the same way for all
//! ROMs. A missing ROM fails the run, whichever ROM it is.
//!
//! There is one "overall" line per system: the mean speed across that system's ROMs
//! (total frames over total emulation time). The Game Boy line covers the colour
//! renderer and double speed as well as the DMG path, and is not comparable with
//! figures from before cgb-acid2 or uCity joined the set (about 2650-2730 fps over
//! the four DMG ROMs). `--min-speed` compares each system's own overall line against
//! its own native rate (both are about 59.73 fps), so one floor guards both cores.
//! The per-ROM lines are unaffected.
//!
//! * `--check` compares each ROM against `crates/gb-test/bench-pins.txt` and fails
//!   if the emulation output differs from the pin, if no pin exists for this
//!   frame count, or if the ROM bytes differ from the pinned build (for example a
//!   homebrew source change; re-pin in that case). The one exception is the agb-built
//!   GBA homebrew (`homebrew/*.gba`), whose ROM-hash pin is informational; see
//!   [`rom_hash_is_informational`].
//! * `--print-pins` prints pin lines for this run, to paste into the pins file.
//! * `--min-speed N` fails if either system's overall speed is below `N` times real time.
//!   CI uses a low floor because shared runners are slow and noisy.

use std::time::{Duration, Instant};

use gb_core::{FRAMES_PER_SECOND, GameBoy, Model};
use gba_core::{Button, Gba};

use gb_test::{roms_dir, workspace_root};

/// What a benchmark ROM runs on.
#[derive(Clone, Copy)]
enum Target {
    Gb(Model),
    /// A GBA ROM; `start` presses START for [`START_FRAMES`] frames from that frame.
    Gba {
        start: Option<u32>,
    },
}

impl Target {
    /// This target's index in [`SYSTEMS`]. An exhaustive match, so a new target
    /// does not compile until it names its slot.
    fn slot(self) -> usize {
        match self {
            Target::Gb(_) => 0,
            Target::Gba { .. } => 1,
        }
    }
}

/// Frame the GBA benchmark ROMs get START, and for how many frames.
const START_AT: u32 = 15;
const START_FRAMES: u32 = 4;

/// ROMs (relative to the ROMs directory) in the benchmark, with what each one
/// runs on.
const ROMS: [(&str, Target); 10] = [
    ("test/dmg-acid2/dmg-acid2.gb", Target::Gb(Model::Dmg)),
    (
        "test/blargg/cpu_instrs/cpu_instrs.gb",
        Target::Gb(Model::Dmg),
    ),
    ("homebrew/snake.gb", Target::Gb(Model::Dmg)),
    ("homebrew/breakout.gb", Target::Gb(Model::Dmg)),
    ("test/cgb-acid2/cgb-acid2.gbc", Target::Gb(Model::Cgb)),
    ("corpus/ucity.gb", Target::Gb(Model::Cgb)),
    ("test/gba/fuzzarm/ARM_Any.gba", Target::Gba { start: None }),
    (
        "homebrew/moth.gba",
        Target::Gba {
            start: Some(START_AT),
        },
    ),
    (
        "homebrew/grains.gba",
        Target::Gba {
            start: Some(START_AT),
        },
    ),
    ("test/gba/tonc/bin/m7_demo.gba", Target::Gba { start: None }),
];

/// The systems in `ROMS`, in the order their overall lines print, with the native
/// frame rate of each (both are about 59.73). Indexed by [`Target::slot`].
const SYSTEMS: [(&str, f64); 2] = [
    ("gb", FRAMES_PER_SECOND),
    ("gba", gba_core::FRAMES_PER_SECOND),
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
// Independent by design: tests/save_state.rs folds its own copy, and the two hashes are never compared.
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

fn run_once(rom: &[u8], target: Target, frames: u32) -> Result<Run, String> {
    match target {
        Target::Gb(model) => run_gb(rom, model, frames),
        Target::Gba { start } => run_gba(rom, start, frames),
    }
}

fn run_gba(rom: &[u8], start: Option<u32>, frames: u32) -> Result<Run, String> {
    let mut gba = Gba::new(rom.to_vec(), None).map_err(|e| e.to_string())?;
    let mut video_hash = FNV_OFFSET;
    let mut audio_hash = FNV_OFFSET;
    let mut elapsed = Duration::ZERO;
    let mut audio = Vec::new();
    for frame in 0..frames {
        if let Some(at) = start {
            if frame == at {
                gba.set_button(Button::Start, true);
            } else if frame == at + START_FRAMES {
                gba.set_button(Button::Start, false);
            }
        }
        let t = Instant::now();
        gba.step_frame();
        audio.clear();
        gba.drain_audio_into(&mut audio);
        elapsed += t.elapsed();
        video_hash = fnv_frame16(video_hash, gba.framebuffer());
        for s in &audio {
            audio_hash = fnv(audio_hash, &s.to_bits().to_le_bytes());
        }
    }
    Ok(Run {
        elapsed,
        video_hash,
        audio_hash,
    })
}

fn run_gb(rom: &[u8], model: Model, frames: u32) -> Result<Run, String> {
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

/// Whether a ROM's bytes are allowed to differ from the pinned build while its video
/// and audio hashes still have to match.
///
/// The GBDK ROMs are byte-identical on every OS, so their ROM hash is binding: it
/// catches a source edit that was not re-pinned. The GBA homebrew is built by a
/// pinned nightly with build-std and fat LTO, and the same sources and toolchain
/// date produce different machine code on Linux x86_64 (CI) than on macOS arm64 (where
/// the pins were taken): the panic-site strings and the code size differ, while the
/// paths in them are identical. The ROM bytes therefore depend on the build host,
/// and only what the emulator does with them can be pinned across hosts.
fn rom_hash_is_informational(name: &str) -> bool {
    name.starts_with("homebrew/") && name.ends_with(".gba")
}

/// Why a ROM's run doesn't match its pin, or `None` if it does. `name` is the ROM's
/// path relative to the ROMs directory.
fn pin_mismatch(name: &str, pin: Option<&Pin>, actual: &Pin) -> Option<&'static str> {
    let output_differs =
        |p: &Pin| (p.video_hash, p.audio_hash) != (actual.video_hash, actual.audio_hash);
    match pin {
        None => Some("no pin for this frame count (add one with --print-pins)"),
        Some(p) if p.rom_hash != actual.rom_hash && !rom_hash_is_informational(name) => {
            Some("ROM differs from the pinned build (rebuilt or edited?); re-pin with --print-pins")
        }
        Some(p) if output_differs(p) && p.rom_hash != actual.rom_hash => Some(
            "emulation output differs from the pin, and so do the ROM bytes (rebuilt, or built on another host)",
        ),
        Some(p) if output_differs(p) => Some("emulation output differs from the pin"),
        Some(_) => None,
    }
}

/// Why a system's overall line and floor check cannot be trusted: not every one of
/// its ROMs completed (a missing ROM or a load error), including none at all.
fn incomplete_system(system: &str, expected: usize, completed: u32) -> Option<String> {
    if completed as usize == expected {
        None
    } else {
        Some(format!(
            "overall ({system}): only {completed} of {expected} ROMs ran, so no speed (or floor) result"
        ))
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
    // Per system: (ROMs run, total emulation time), in `SYSTEMS` order.
    let mut totals = [(0u32, Duration::ZERO); SYSTEMS.len()];
    for (name, target) in ROMS {
        let path = roms_dir().join(name);
        let Ok(rom) = std::fs::read(&path) else {
            println!("{name:<38} missing ({})", path.display());
            ok = false;
            continue;
        };
        let mut best: Option<Run> = None;
        for _ in 0..repeats {
            match run_once(&rom, target, frames) {
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
        let slot = target.slot();
        totals[slot].0 += 1;
        totals[slot].1 += r.elapsed;
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
            match pin_mismatch(name, pin, &actual) {
                Some(why) => {
                    ok = false;
                    format!("  MISMATCH: {why}")
                }
                None if pin.is_some_and(|p| p.rom_hash != actual.rom_hash) => {
                    "  pinned (ROM bytes differ from the pin; video and audio match)".to_string()
                }
                None => "  pinned".to_string(),
            }
        } else {
            String::new()
        };
        println!(
            "{name:<38} {fps:>10.0} {:>9.1}x  {:016x}  {:016x}{verdict}",
            fps / SYSTEMS[slot].1,
            r.video_hash,
            r.audio_hash
        );
        pin_lines.push(format!(
            "{frames} {:016x} {:016x} {:016x} {name}",
            actual.rom_hash, actual.video_hash, actual.audio_hash
        ));
    }
    for (slot, (&(system, native), &(roms, total))) in SYSTEMS.iter().zip(&totals).enumerate() {
        let expected = ROMS.iter().filter(|(_, t)| t.slot() == slot).count();
        if let Some(why) = incomplete_system(system, expected, roms) {
            // A ROM that was missing or failed already failed the run; say what that
            // means for this system's speed line, and never skip its floor check.
            println!("FAIL: {why}");
            ok = false;
            continue;
        }
        let all = f64::from(frames) * f64::from(roms) / total.as_secs_f64();
        let speed = all / native;
        println!(
            "{:<38} {all:>10.0} {speed:>9.1}x",
            format!("overall ({system})")
        );
        if let Some(floor) = min_speed {
            if speed < floor {
                println!(
                    "FAIL: overall ({system}) {speed:.1}x is below the --min-speed floor of {floor}x"
                );
                ok = false;
            } else {
                println!("speed floor {floor}x ({system}): ok");
            }
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

    /// A bare `--bench --check` runs DEFAULT_FRAMES and CI runs 1200: every ROM needs
    /// a pin at both, or the check fails for want of one.
    #[test]
    fn every_rom_is_pinned_at_the_default_and_the_ci_frame_counts() {
        let text = std::fs::read_to_string(workspace_root().join(PINS_FILE)).unwrap();
        let pins = parse_pins(&text).unwrap();
        for (name, _) in ROMS {
            for frames in [1200, DEFAULT_FRAMES] {
                assert!(
                    pins.iter().any(|(f, p, _)| *f == frames && p == name),
                    "{name} has no pin at {frames} frames"
                );
            }
        }
    }

    #[test]
    fn a_system_with_a_missing_rom_or_none_fails_instead_of_skipping() {
        assert_eq!(incomplete_system("gba", 4, 4), None);
        assert!(incomplete_system("gba", 4, 3).unwrap().contains("3 of 4"));
        assert!(incomplete_system("gba", 4, 0).unwrap().contains("0 of 4"));
    }

    #[test]
    fn mismatch_reasons_are_distinct() {
        let gb = "homebrew/snake.gb";
        assert_eq!(pin_mismatch(gb, Some(&PIN), &PIN), None);
        assert!(pin_mismatch(gb, None, &PIN).unwrap().contains("no pin"));
        let rom = Pin { rom_hash: 9, ..PIN };
        assert!(
            pin_mismatch(gb, Some(&PIN), &rom)
                .unwrap()
                .contains("ROM differs")
        );
        let out = Pin {
            audio_hash: 9,
            ..PIN
        };
        assert!(
            pin_mismatch(gb, Some(&PIN), &out)
                .unwrap()
                .contains("output differs")
        );
    }

    #[test]
    fn gba_homebrew_rom_bytes_may_differ_but_its_output_may_not() {
        let gba = "homebrew/moth.gba";
        let rom = Pin { rom_hash: 9, ..PIN };
        assert_eq!(pin_mismatch(gba, Some(&PIN), &rom), None);
        let both = Pin {
            rom_hash: 9,
            video_hash: 9,
            ..PIN
        };
        assert!(
            pin_mismatch(gba, Some(&PIN), &both)
                .unwrap()
                .contains("output differs")
        );
        assert!(pin_mismatch(gba, None, &rom).unwrap().contains("no pin"));
        assert!(!rom_hash_is_informational("homebrew/snake.gb"));
        assert!(!rom_hash_is_informational("test/gba/fuzzarm/ARM_Any.gba"));
    }
}
