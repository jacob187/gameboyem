//! GBA save-state determinism over real ROMs: saving at frame N (or part-way
//! through a frame), loading the state into a fresh machine and playing the same
//! input must reproduce the original run's frames, audio and cycle count bit for
//! bit, and end in a byte-identical state. A field missing from the state shows
//! up here as a divergence. Mirrors `save_state.rs` for the Game Boy.
//!
//! ROMs that aren't present locally are skipped. In CI (`CI` set), finding none
//! at all is a failure.

use std::path::PathBuf;

use gb_test::{in_ci, roms_dir};
use gba_core::{Button, Gba};

const BEFORE: u32 = 60;
const AFTER: u32 = 60;
/// Where to save besides the frame boundary: this many lines into the frame
/// after the one `BEFORE` ends (VBlank starts at line 160).
const MID_FRAME_LINE: u64 = 85;

/// The ROMs: test suites, tonc demos that move on their own, and the two
/// homebrew games in `roms/homebrew` (moth plays sound once START is pressed,
/// grains reacts to the D-pad).
const ROMS: [&str; 10] = [
    "test/gba/jsmolka/arm/arm.gba",
    "test/gba/fuzzarm/FuzzARM.gba",
    "test/gba/tonc/bin/m7_demo.gba",
    "test/gba/tonc/bin/irq_demo.gba",
    "test/gba/tonc/bin/dma_demo.gba",
    "test/gba/tonc/bin/snd1_demo.gba",
    "test/gba/tonc/bin/key_demo.gba",
    "test/gba/jsmolka/save/flash128.gba",
    "homebrew/moth.gba",
    "homebrew/grains.gba",
];

fn roms() -> Vec<PathBuf> {
    let root = roms_dir();
    ROMS.iter()
        .map(|p| root.join(p))
        .filter(|p| p.is_file())
        .collect()
}

/// Deterministic input: START pulses (menus, moth's title), A, and a D-pad that
/// sweeps, so games react.
fn apply_input(g: &mut Gba, frame: u32) {
    g.set_button(Button::Start, frame % 45 < 3);
    g.set_button(Button::A, frame % 17 < 3);
    g.set_button(Button::Right, (frame / 20) % 2 == 0);
    g.set_button(Button::Left, (frame / 20) % 2 == 1);
    g.set_button(Button::Up, frame % 31 < 6);
    g.set_button(Button::R, frame % 23 < 2);
}

fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[derive(Debug, PartialEq)]
struct Run {
    video: u64,
    audio: u64,
    cycles: u64,
    /// Distinct frames seen and the loudest sample: evidence the run did something.
    distinct: usize,
    peak: f32,
}

/// Hash every frame and the audio over `frames` frames of scripted input from
/// frame `start`, with `input` applied before each.
fn run(g: &mut Gba, start: u32, frames: u32, input: fn(&mut Gba, u32)) -> Run {
    let (mut video, mut audio) = (0xcbf2_9ce4_8422_2325, 0xcbf2_9ce4_8422_2325);
    let mut samples = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut peak = 0f32;
    for f in start..start + frames {
        input(g, f);
        g.step_frame();
        let bytes: Vec<u8> = g
            .framebuffer()
            .iter()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        video = fnv(video, &bytes);
        seen.insert(fnv(0xcbf2_9ce4_8422_2325, &bytes));
        samples.clear();
        g.drain_audio_into(&mut samples);
        for s in &samples {
            audio = fnv(audio, &s.to_bits().to_le_bytes());
            peak = peak.max(s.abs());
        }
    }
    Run {
        video,
        audio,
        cycles: g.cycles(),
        distinct: seen.len(),
        peak,
    }
}

/// Save `a` here, play `AFTER` frames from it and from a fresh machine that
/// loaded the state, and compare everything. Returns the run for the callers
/// that check it did something.
fn replay(mut a: Gba, rom: &[u8], frame: u32, input: fn(&mut Gba, u32)) -> Result<Run, String> {
    // Host buffers aren't part of a state; start both runs with them empty and
    // the APU brought up to the clock.
    a.drain_audio_into(&mut Vec::new());
    let snap = a.save_state();
    if snap.len() > gba_core::MAX_STATE_BYTES {
        return Err(format!("state of {} bytes exceeds the cap", snap.len()));
    }
    let expected = run(&mut a, frame, AFTER, input);

    let mut b = Gba::new(rom.to_vec(), None).map_err(|e| format!("{e:?}"))?;
    b.load_state(&snap).map_err(|e| e.to_string())?;
    if b.save_state() != snap {
        return Err("load then save is not byte-identical".into());
    }
    let actual = run(&mut b, frame, AFTER, input);
    if actual.video != expected.video {
        return Err("video diverged after load".into());
    }
    if actual.audio != expected.audio {
        return Err("audio diverged after load".into());
    }
    if actual.cycles != expected.cycles {
        return Err(format!(
            "cycle counts differ: {} vs {}",
            actual.cycles, expected.cycles
        ));
    }
    if a.save_state() != b.save_state() {
        return Err("final states differ".into());
    }
    Ok(expected)
}

/// The scripted input for `rom`: moth waits on START, so it also gets a long
/// press early on.
fn input_for(path: &std::path::Path) -> fn(&mut Gba, u32) {
    if path.ends_with("moth.gba") {
        |g, f| {
            apply_input(g, f);
            g.set_button(Button::Start, (5..12).contains(&f) || f % 45 < 3);
        }
    } else {
        apply_input
    }
}

fn check(path: &PathBuf, mid_frame: bool) -> Result<Run, String> {
    let rom = std::fs::read(path).map_err(|e| e.to_string())?;
    let input = input_for(path);
    let mut a = Gba::new(rom.clone(), None).map_err(|e| format!("{e:?}"))?;
    run(&mut a, 0, BEFORE, input);
    if mid_frame {
        let target = a.cycles() + MID_FRAME_LINE * u64::from(gba_core::CYCLES_PER_LINE);
        while a.cycles() < target {
            a.step_instruction();
        }
    }
    // After a mid-frame save the next `step_frame` finishes the frame in progress.
    replay(a, &rom, BEFORE, input)
}

#[test]
fn gba_save_state_replays_identically() {
    let roms = roms();
    if roms.is_empty() {
        assert!(!in_ci(), "no ROMs found under {}", roms_dir().display());
        eprintln!("no ROMs found; skipping");
        return;
    }
    type Outcome = (PathBuf, bool, Result<Run, String>);
    let outcomes: Vec<Outcome> = std::thread::scope(|s| {
        let handles: Vec<_> = roms
            .iter()
            .flat_map(|p| [false, true].map(|m| (p, m)))
            .map(|(p, mid)| s.spawn(move || (p.clone(), mid, check(p, mid))))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("ROM thread panicked"))
            .collect()
    });
    let failures: Vec<String> = outcomes
        .iter()
        .filter_map(|(p, mid, r)| {
            r.as_ref()
                .err()
                .map(|e| format!("{} (mid-frame {mid}): {e}", p.display()))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));

    // The runs did something, so the comparison above is not vacuous.
    for (p, _, r) in &outcomes {
        let r = r.as_ref().unwrap();
        let name = p.file_name().unwrap().to_string_lossy();
        eprintln!("{name}: {} distinct frames, peak {}", r.distinct, r.peak);
        match &*name {
            "m7_demo.gba" | "key_demo.gba" | "dma_demo.gba" | "irq_demo.gba" | "FuzzARM.gba"
            | "grains.gba" => {
                assert!(r.distinct > 1, "{name} showed one frame only")
            }
            "moth.gba" | "snd1_demo.gba" => {
                assert!(r.peak > 0.001, "{name} was silent (peak {})", r.peak)
            }
            _ => {}
        }
    }
    eprintln!("{} GBA ROMs replayed identically", roms.len());
}

/// grains reacts to input: from the same state, a run with the D-pad held and a
/// run with nothing pressed show different pictures. Without this the test above
/// would pass for a core that ignored the keypad state in the snapshot.
#[test]
fn grains_frame_depends_on_input() {
    let path = roms_dir().join("homebrew/grains.gba");
    let Ok(rom) = std::fs::read(&path) else {
        assert!(!in_ci(), "grains.gba is missing");
        eprintln!("grains.gba not found; skipping");
        return;
    };
    let mut a = Gba::new(rom, None).unwrap();
    run(&mut a, 0, BEFORE, apply_input);
    let snap = a.save_state();
    // A machine of the same ROM, loaded with the state, then two futures.
    let mut b = Gba::new(std::fs::read(&path).unwrap(), None).unwrap();
    b.load_state(&snap).unwrap();
    let idle = run(&mut a, BEFORE, AFTER, |g, _| {
        for b in [
            Button::Left,
            Button::Right,
            Button::Up,
            Button::Down,
            Button::A,
        ] {
            g.set_button(b, false);
        }
    });
    let busy = run(&mut b, BEFORE, AFTER, apply_input);
    assert_ne!(
        idle.video, busy.video,
        "the input changed nothing on screen"
    );
}

/// moth plays sound after START: from a state taken before START, the run with
/// START pressed is audible and the run without it is not.
#[test]
fn moth_sound_depends_on_start() {
    let path = roms_dir().join("homebrew/moth.gba");
    let Ok(rom) = std::fs::read(&path) else {
        assert!(!in_ci(), "moth.gba is missing");
        eprintln!("moth.gba not found; skipping");
        return;
    };
    let mut a = Gba::new(rom.clone(), None).unwrap();
    run(&mut a, 0, 20, |_, _| {});
    a.drain_audio_into(&mut Vec::new());
    let snap = a.save_state();
    let mut b = Gba::new(rom, None).unwrap();
    b.load_state(&snap).unwrap();
    let quiet = run(&mut a, 20, 90, |_, _| {});
    let played = run(&mut b, 20, 90, |g, f| {
        g.set_button(Button::Start, f < 26);
    });
    assert_ne!(quiet.audio, played.audio, "START changed nothing audible");
    assert!(
        played.peak > quiet.peak,
        "{} vs {}",
        played.peak,
        quiet.peak
    );
}
