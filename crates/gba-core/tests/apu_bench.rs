//! Cost of the APU per emulated frame. Run with
//! `cargo test --release -p gba-core --test apu_bench -- --ignored --nocapture`.

mod apu_common;
use apu_common::*;
use gba_core::CYCLES_PER_FRAME;
use std::time::Instant;

const FRAMES: usize = 600;

/// Total time to run the scene, with the APU and without it (sound registers
/// at their power-on values, timers and DMA still running the same loop).
fn time_run(sound: bool) -> f64 {
    let mut bus = new_bus();
    if sound {
        start_scene(&mut bus);
    }
    let mut out = Vec::with_capacity(2 * 48_000);
    let t = Instant::now();
    for _ in 0..FRAMES {
        out.clear();
        run_frame_audio(&mut bus, &mut out);
        std::hint::black_box(&out);
    }
    t.elapsed().as_secs_f64()
}

#[test]
#[ignore = "benchmark"]
fn apu_cost_per_frame() {
    let best = |sound| (0..3).map(|_| time_run(sound)).fold(f64::MAX, f64::min);
    let (with, without) = (best(true), best(false));
    let per_frame = |t: f64| t / FRAMES as f64 * 1e6;
    println!(
        "scene (4 PSG + 2 FIFO + timers + DMA): {:.1} us/frame; idle APU, no sound: {:.1} us/frame; \
         frame budget {:.0} us; best of 3 over {FRAMES} frames of {CYCLES_PER_FRAME} cycles",
        per_frame(with),
        per_frame(without),
        1e6 / gba_core::FRAMES_PER_SECOND,
    );
}
