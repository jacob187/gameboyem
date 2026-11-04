//! Headless speed benchmark: `cargo run -p gb-test --release -- --bench [FRAMES] [REPEATS]`.
//!
//! Runs a fixed number of frames (default 3600) of a few representative ROMs and
//! reports emulated frames/sec and the speed relative to real time (59.73 fps).
//! Nothing here opens a window or an audio device; audio is only drained and
//! hashed. Each ROM also prints an FNV-1a hash of every frame's framebuffer
//! plus the serial output, and a separate hash of the audio samples, so a
//! performance change can be checked for bit-identical behaviour by comparing
//! the hashes before and after. Not part of `cargo test`.

use std::time::{Duration, Instant};

use gb_core::{FRAMES_PER_SECOND, GameBoy};

use gb_test::roms_dir;

/// ROMs (relative to the ROMs directory) in the benchmark.
const ROMS: [&str; 4] = [
    "test/dmg-acid2/dmg-acid2.gb",
    "test/blargg/cpu_instrs/cpu_instrs.gb",
    "homebrew/snake.gb",
    "homebrew/breakout.gb",
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

struct Run {
    elapsed: Duration,
    video_hash: u64,
    audio_hash: u64,
}

fn run_once(rom: &[u8], frames: u32) -> Result<Run, String> {
    let mut gb = GameBoy::new(rom.to_vec()).map_err(|e| e.to_string())?;
    let mut video_hash = FNV_OFFSET;
    let mut audio_hash = FNV_OFFSET;
    let mut elapsed = Duration::ZERO;
    for _ in 0..frames {
        let t = Instant::now();
        gb.step_frame();
        let audio = gb.audio_samples();
        elapsed += t.elapsed();
        video_hash = fnv(video_hash, gb.framebuffer());
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

/// Entry point for `--bench`. `args` are the positional arguments after the flag.
pub fn run(args: &[&String]) -> std::process::ExitCode {
    let parse = |i: usize, default: u32| {
        args.get(i)
            .and_then(|a| a.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(default)
    };
    let frames = parse(0, DEFAULT_FRAMES);
    let repeats = parse(1, DEFAULT_REPEATS);
    println!("{frames} frames per ROM, best of {repeats}; real time = {FRAMES_PER_SECOND:.2} fps");
    println!(
        "{:<38} {:>10} {:>10}  {:<17} {:<16}",
        "rom", "frames/s", "realtime", "video+serial hash", "audio hash"
    );
    let mut ok = true;
    let mut total = Duration::ZERO;
    for name in ROMS {
        let path = roms_dir().join(name);
        let Ok(rom) = std::fs::read(&path) else {
            println!("{name:<38} missing ({})", path.display());
            ok = false;
            continue;
        };
        let mut best: Option<Run> = None;
        for _ in 0..repeats {
            match run_once(&rom, frames) {
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
        println!(
            "{name:<38} {fps:>10.0} {:>9.1}x  {:016x}  {:016x}",
            fps / FRAMES_PER_SECOND,
            r.video_hash,
            r.audio_hash
        );
    }
    let all = f64::from(frames) * ROMS.len() as f64 / total.as_secs_f64();
    println!(
        "{:<38} {all:>10.0} {:>9.1}x",
        "overall",
        all / FRAMES_PER_SECOND
    );
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
