//! Save-state determinism over real ROMs: saving at frame N, loading the state
//! into a fresh machine and replaying the same input must reproduce the
//! original run's frames, audio and serial output bit for bit. A field missing
//! from the state shows up here as a divergence.
//!
//! ROMs that aren't present locally are skipped. In CI (`CI` set), finding none
//! at all is a failure.

use std::path::PathBuf;

use gb_core::{Button, GameBoy, Model};
use gb_test::{in_ci, roms_dir};

const BEFORE: u32 = 240;
const AFTER: u32 = 240;
/// Where to save, in M-cycles after `step_frame` returns (the start of VBlank,
/// line 144): lines 50 and 140 of the next frame. Mid-frame saves put per-line
/// and per-frame PPU state (window line, sprite buffer, ...) in the snapshot,
/// and line 140 falls inside the bottom status bars games draw with the window.
const SAVE_OFFSETS: [u64; 2] = [(10 + 50) * 114, (10 + 140) * 114];

/// A ROM and the console to run it on.
type Case = (PathBuf, Model);

fn roms() -> Vec<Case> {
    let root = roms_dir();
    let mut out: Vec<PathBuf> = ["corpus", "homebrew"]
        .iter()
        .filter_map(|d| std::fs::read_dir(root.join(d)).ok())
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "gb"))
        .collect();
    out.extend(
        [
            "test/blargg/dmg_sound/dmg_sound.gb",
            "test/blargg/cpu_instrs/cpu_instrs.gb",
            "test/dmg-acid2/dmg-acid2.gb",
            "test/mooneye/emulator-only/mbc5/rom_8Mb.gb",
        ]
        .iter()
        .map(|p| root.join(p))
        .filter(|p| p.is_file()),
    );
    out.sort();
    let mut cases: Vec<Case> = out.into_iter().map(|p| (p, Model::Dmg)).collect();
    // CGB colour mode (cgb-acid2: RGB555 frames, palette RAM, attributes; Aevilia, a CGB-only
    // MBC5 RPG with 128 KiB of cartridge RAM, from the corpus) and a flag-less ROM on a CGB
    // (DMG-compatibility mode).
    cases.extend(
        [
            ("test/cgb-acid2/cgb-acid2.gbc", Model::Cgb),
            ("corpus/aevilia.gb", Model::Cgb),
            ("test/dmg-acid2/dmg-acid2.gb", Model::Cgb),
        ]
        .map(|(p, m)| (root.join(p), m))
        .into_iter()
        .filter(|(p, _)| p.is_file()),
    );
    cases
}

/// Deterministic input so menus advance and games react.
fn apply_input(gb: &mut GameBoy, frame: u32) {
    gb.set_button(Button::Start, frame % 45 < 2);
    gb.set_button(Button::A, frame % 17 < 3);
    gb.set_button(Button::Right, (frame / 60) % 2 == 0);
    gb.set_button(Button::Left, (frame / 60) % 2 == 1);
    gb.set_button(Button::Up, frame % 90 > 80);
}

/// Hash of every frame, the audio and the serial output over `frames` frames.
fn run(gb: &mut GameBoy, start: u32, frames: u32) -> (u64, u64, Vec<u8>) {
    let fnv = |mut h: u64, bytes: &[u8]| {
        for &b in bytes {
            h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    };
    let (mut video, mut audio) = (0xcbf2_9ce4_8422_2325, 0xcbf2_9ce4_8422_2325);
    let mut samples = Vec::new();
    for f in start..start + frames {
        apply_input(gb, f);
        gb.step_frame();
        video = fnv(video, gb.framebuffer());
        if let Some(fb) = gb.cgb_framebuffer() {
            for px in fb {
                video = fnv(video, &px.to_le_bytes());
            }
        }
        samples.clear();
        gb.drain_audio_into(&mut samples);
        for s in &samples {
            audio = fnv(audio, &s.to_bits().to_le_bytes());
        }
    }
    (video, audio, gb.take_serial_output())
}

fn check((path, model): &Case, offset: u64) -> Result<(), String> {
    let rom = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut a = GameBoy::with_model(rom.clone(), *model).map_err(|e| format!("{e:?}"))?;
    run(&mut a, 0, BEFORE);
    let target = a.mcycles() + offset;
    while a.mcycles() < target {
        a.step_instruction();
    }
    // Host buffers aren't part of a state; start both runs with them empty.
    a.audio_samples();
    a.take_serial_output();
    let snap = a.save_state();
    let expected = run(&mut a, BEFORE, AFTER);

    let mut b = GameBoy::with_model(rom, *model).map_err(|e| format!("{e:?}"))?;
    b.load_state(&snap).map_err(|e| e.to_string())?;
    if b.save_state() != snap {
        return Err("load then save is not byte-identical".into());
    }
    let actual = run(&mut b, BEFORE, AFTER);
    if actual.0 != expected.0 {
        return Err("video diverged after load".into());
    }
    if actual.1 != expected.1 {
        return Err("audio diverged after load".into());
    }
    if actual.2 != expected.2 {
        return Err("serial output diverged after load".into());
    }
    if a.save_state() != b.save_state() {
        return Err("final states differ".into());
    }
    Ok(())
}

#[test]
fn save_state_replays_identically() {
    let roms = roms();
    if roms.is_empty() {
        assert!(!in_ci(), "no ROMs found under {}", roms_dir().display());
        eprintln!("no ROMs found; skipping");
        return;
    }
    let failures: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = roms
            .iter()
            .flat_map(|p| SAVE_OFFSETS.map(|o| (p, o)))
            .map(|(c, o)| {
                s.spawn(move || {
                    check(c, o)
                        .err()
                        .map(|e| format!("{} ({:?}, offset {o}): {e}", c.0.display(), c.1))
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().expect("ROM thread panicked"))
            .collect()
    });
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    eprintln!("{} ROMs replayed identically", roms.len());
}
