//! Audio from real homebrew. Needs the CPU (P1-CPU), so it is `#[ignore]`d on
//! this branch; run with `cargo test -p gba-core --test apu_roms -- --ignored`.

use gba_core::{Button, Gba};
use std::path::Path;

const MOTH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../roms/homebrew/moth.gba");

/// Largest deviation of either lane from `base`.
fn deviation(samples: &[f32], base: (f32, f32)) -> (f32, f32) {
    let dev = |lane: usize, b: f32| {
        samples
            .iter()
            .skip(lane)
            .step_by(2)
            .fold(0.0f32, |m, &s| m.max((s - b).abs()))
    };
    (dev(0, base.0), dev(1, base.1))
}

/// MOTH mixes in software (agb's mixer, 10512 Hz) and plays it through Direct
/// Sound DMA. Per `homebrew/gba/README.md`: nothing plays on the title; START
/// begins a round and a rising jingle of about 0.34 s must make the output
/// non-silent on both channels.
#[test]
// The CPU is on `feat/gba` now; the coordinator enables this after merging it.
#[ignore = "needs P1-CPU"]
fn moth_plays_its_jingle_after_start() {
    let Ok(rom) = std::fs::read(Path::new(MOTH)) else {
        eprintln!("skipping: {MOTH} not found");
        return;
    };
    let mut gba = Gba::new(rom, None).unwrap();
    let mut s = vec![];
    // The title: wait 10 frames (agb boot takes a few), no audio yet.
    for _ in 0..10 {
        gba.step_frame();
    }
    gba.drain_audio_into(&mut s);
    let n = s.len();
    assert!(n >= 4, "audio is produced from power-on");
    let base = (s[n - 2], s[n - 1]);
    let (dl, dr) = deviation(&s[n.saturating_sub(2 * 800)..], base);
    assert!(dl < 0.01 && dr < 0.01, "title is silent: {dl} {dr}");

    // Hold START for 3 frames, then let the jingle play.
    gba.set_button(Button::Start, true);
    for _ in 0..3 {
        gba.step_frame();
    }
    gba.set_button(Button::Start, false);
    for _ in 0..27 {
        gba.step_frame();
    }
    s.clear();
    gba.drain_audio_into(&mut s);
    let (dl, dr) = deviation(&s, base);
    assert!(dl > 0.02 && dr > 0.02, "jingle is audible: {dl} {dr}");
    assert!(s.iter().all(|x| (-1.0..=1.0).contains(x)));
}
