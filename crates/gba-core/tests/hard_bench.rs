//! A print-only benchmark for the hardening work, `#[ignore]`d so it never gates
//! (timing varies with machine load): `cargo test --release -p gba-core --test
//! hard_bench -- --ignored --nocapture --test-threads=1`. Best of three runs each.

mod sys_common;

use gba_core::Gba;
use gba_core::bus::CpuBus;
use std::path::Path;
use std::time::Instant;
use sys_common::{N, bus, run};

const ROMS: [&str; 4] = [
    "roms/test/gba/fuzzarm/ARM_Any.gba",
    "roms/homebrew/moth.gba",
    "roms/homebrew/grains.gba",
    "roms/test/gba/tonc/bin/m7_demo.gba",
];

fn best_of_three(mut f: impl FnMut() -> f64) -> f64 {
    (0..3).map(|_| f()).fold(f64::INFINITY, f64::min)
}

#[test]
#[ignore = "benchmark: prints frames per second"]
fn bench_fps() {
    for rom in ROMS {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rom);
        let Ok(data) = std::fs::read(&path) else {
            eprintln!("skipping {rom}: not found");
            continue;
        };
        let frames = 600u32;
        let secs = best_of_three(|| {
            let mut gba = Gba::new(data.clone(), None).unwrap();
            let t = Instant::now();
            for _ in 0..frames {
                gba.step_frame();
            }
            t.elapsed().as_secs_f64()
        });
        println!("{rom}: {:.0} fps", f64::from(frames) / secs);
    }
}

/// Four prescaler-1 timers at reload 0xFFFF (an overflow every cycle) for ten
/// frames of emulated time, driven through the bus in instruction-sized steps.
/// `ctl` is the control half-word written to each timer (enable is added).
fn storm(ctl: [u32; 4]) -> f64 {
    best_of_three(|| {
        let mut b = bus();
        for (n, c) in ctl.into_iter().enumerate() {
            b.write32(0x0400_0100 + 4 * n as u32, 0xFFFF | (c | 0x80) << 16, N);
        }
        let t = Instant::now();
        for _ in 0..(10 * 280_896 / 4) {
            run(&mut b, 4);
        }
        t.elapsed().as_secs_f64()
    })
}

#[test]
#[ignore = "benchmark: prints seconds per ten emulated frames"]
fn bench_timer_storm() {
    println!("unobserved storm: {:.3} s", storm([0, 0, 0, 0]));
    println!("IRQ-enabled storm: {:.3} s", storm([0x40; 4]));
    println!("cascade chain storm: {:.3} s", storm([0, 0x4, 0x4, 0x44]));
}
