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
/// Where to save, in lines after `step_frame` returns (the start of VBlank,
/// line 144): lines 50 and 140 of the next frame. Mid-frame saves put per-line
/// and per-frame PPU state (window line, sprite buffer, ...) in the snapshot,
/// and line 140 falls inside the bottom status bars games draw with the window.
const SAVE_LINES: [u64; 2] = [10 + 50, 10 + 140];

/// CPU M-cycles per line (456 dots): 114 at normal speed, 228 in double speed, where the CPU
/// runs twice as fast against the PPU.
fn mcycles_per_line(gb: &GameBoy) -> u64 {
    if gb.double_speed() { 228 } else { 114 }
}

/// Corpus ROMs that need a CGB (colour renderer, VRAM banks, palette RAM, double speed), and
/// cgbdemo from `homebrew/`. They replay as `Model::Cgb`; the other corpus ROMs replay as DMG.
const CGB_ROMS: [&str; 7] = [
    "corpus/aevilia.gb",
    "corpus/escape-obliterizer-dx.gb",
    "corpus/geometrix.gb",
    "corpus/rebound.gb",
    "corpus/tobutobugirl-dx.gb",
    "corpus/ucity.gb",
    "homebrew/cgbdemo.gb",
];

/// ROMs that must be in double speed at the save point, so that case really replays that
/// clocking path (uCity switches at about frame 15).
const DOUBLE_SPEED_ROMS: [&str; 1] = ["corpus/ucity.gb"];

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
    let cgb_roms = CGB_ROMS.map(|p| root.join(p));
    let mut cases: Vec<Case> = out
        .into_iter()
        .map(|p| {
            let model = if cgb_roms.contains(&p) {
                Model::Cgb
            } else {
                Model::Dmg
            };
            (p, model)
        })
        .collect();
    // CGB colour mode (cgb-acid2: RGB555 frames, palette RAM, attributes; the corpus and
    // homebrew CGB games above, from the globs) and a flag-less ROM on a CGB
    // (DMG-compatibility mode).
    cases.extend(
        [
            ("test/cgb-acid2/cgb-acid2.gbc", Model::Cgb),
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
            // Independent by design: bench.rs folds its own copy, and the two hashes are never compared.
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

fn check((path, model): &Case, lines: u64) -> Result<(), String> {
    let rom = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut a = GameBoy::with_model(rom.clone(), *model).map_err(|e| format!("{e:?}"))?;
    run(&mut a, 0, BEFORE);
    let root = roms_dir();
    if DOUBLE_SPEED_ROMS.iter().any(|p| root.join(p) == *path) && !a.double_speed() {
        return Err(format!("not in double speed after {BEFORE} frames"));
    }
    let target = a.mcycles() + lines * mcycles_per_line(&a);
    while a.mcycles() < target {
        a.step_instruction();
    }
    replay(a, rom, *model, BEFORE)
}

/// Save `a` here, replay `AFTER` frames from it and from a fresh machine that
/// loaded the state, and compare everything. `frame` is the frame counter to
/// continue the scripted input from.
fn replay(mut a: GameBoy, rom: Vec<u8>, model: Model, frame: u32) -> Result<(), String> {
    // Host buffers aren't part of a state; start both runs with them empty.
    a.audio_samples();
    a.take_serial_output();
    let snap = a.save_state();
    let expected = run(&mut a, frame, AFTER);

    let mut b = GameBoy::with_model(rom, model).map_err(|e| format!("{e:?}"))?;
    b.load_state(&snap).map_err(|e| e.to_string())?;
    if b.save_state() != snap {
        return Err("load then save is not byte-identical".into());
    }
    let actual = run(&mut b, frame, AFTER);
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
            .flat_map(|p| SAVE_LINES.map(|l| (p, l)))
            .map(|(c, l)| {
                s.spawn(move || {
                    check(c, l)
                        .err()
                        .map(|e| format!("{} ({:?}, line {l}): {e}", c.0.display(), c.1))
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

/// A CGB-flagged ROM that switches to double speed, then starts an HBlank DMA of
/// 128 blocks from ROM into VRAM, then spins.
fn hdma_double_speed_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    rom[0x100..0x103].copy_from_slice(&[0xC3, 0x50, 0x01]); // JP 0150
    rom[0x143] = 0xC0;
    #[rustfmt::skip]
    let code = [
        0xF3,             // DI
        0x3E, 0x01, 0xE0, 0x4D, // LD A,1 ; LDH (KEY1),A
        0x10, 0x00,       // STOP: switch to double speed
        0x3E, 0x01, 0xE0, 0x51, // HDMA1: source 0x0100
        0x3E, 0x00, 0xE0, 0x52, // HDMA2
        0x3E, 0x80, 0xE0, 0x53, // HDMA3: destination 0x8000
        0x3E, 0x00, 0xE0, 0x54, // HDMA4
        0x3E, 0xFF, 0xE0, 0x55, // HDMA5: HBlank mode, 128 blocks
        0x04,             // loop: INC B
        0x18, 0xFD,       // JR loop
    ];
    rom[0x150..0x150 + code.len()].copy_from_slice(&code);
    rom
}

#[test]
fn save_state_replays_identically_mid_hdma_in_double_speed() {
    let rom = hdma_double_speed_rom();
    let mut gb = GameBoy::with_model(rom.clone(), Model::Cgb).expect("synthetic ROM loads");
    // FF4D bit 7 = current speed; FF55 bit 7 clear = an HBlank DMA is running.
    let mut guard = 0;
    while gb.peek(0xFF55) & 0x80 != 0 {
        gb.step_instruction();
        guard += 1;
        assert!(guard < 1_000_000, "the HDMA never started");
    }
    // Let it copy a few blocks, then save in the middle of it.
    let target = gb.mcycles() + 20 * 228;
    while gb.mcycles() < target {
        gb.step_instruction();
    }
    assert_eq!(gb.peek(0xFF4D) & 0x80, 0x80, "in double speed");
    assert_eq!(gb.peek(0xFF55) & 0x80, 0, "HDMA still running");
    assert!(gb.double_speed());
    replay(gb, rom, Model::Cgb, 0).expect("replay");
}
