//! Task 4: audio output is finite and within the documented -1.0..=1.0 range,
//! and the number of stereo frames produced tracks the configured sample rate
//! (within +-2) for every rate from 8 kHz to 192 kHz.

mod common;

use common::*;
use gb_core::{GameBoy, M_CYCLES_PER_FRAME};

/// M-cycles per second (the APU's sampling clock).
const MCYCLES_PER_SECOND: u64 = 1 << 20;

fn expected_pairs(rate: u32, mcycles: u64) -> f64 {
    f64::from(rate) * mcycles as f64 / MCYCLES_PER_SECOND as f64
}

/// All four channels at full volume, centred, plus wave RAM: loud and busy.
fn tone_program() -> Vec<u8> {
    let mut p = Program::default();
    p.write(0xFF26, 0x80)
        .write(0xFF24, 0x77)
        .write(0xFF25, 0xFF);
    p.write(0xFF11, 0x80)
        .write(0xFF12, 0xF0)
        .write(0xFF13, 0x00)
        .write(0xFF14, 0x87);
    p.write(0xFF16, 0x40)
        .write(0xFF17, 0xF0)
        .write(0xFF18, 0x80)
        .write(0xFF19, 0x87);
    p.write(0xFF1A, 0x80).write(0xFF1C, 0x20);
    for i in 0..16u16 {
        p.write(0xFF30 + i, (i as u8).wrapping_mul(0x11));
    }
    p.write(0xFF1D, 0x00).write(0xFF1E, 0x87);
    p.write(0xFF21, 0xF0)
        .write(0xFF22, 0x33)
        .write(0xFF23, 0x80);
    p.spin();
    p.0
}

/// Random APU register stream with delays, retriggers and power cycling.
fn noisy_apu_program(rng: &mut Rng) -> Vec<u8> {
    let mut p = Program::default();
    p.write(0xFF26, 0x80)
        .write(0xFF24, rng.u8())
        .write(0xFF25, rng.u8());
    for _ in 0..500 {
        match rng.below(10) {
            0 => {
                p.delay(rng.u8());
            }
            1 => {
                // Trigger a random channel.
                let nrx4 = rng.pick(&[0xFF14u16, 0xFF19, 0xFF1E, 0xFF23]);
                p.write(nrx4, 0x80 | rng.u8());
            }
            2 => {
                // Power the APU off/on (clears registers, silences output).
                p.write(0xFF26, rng.u8());
            }
            3..=5 => {
                p.write(0xFF30 + rng.below(16) as u16, rng.u8());
            }
            _ => {
                p.write(0xFF10 + rng.below(0x17) as u16, rng.u8());
            }
        }
        if p.len() + 64 > MAX_PROGRAM {
            break;
        }
    }
    p.write(0xFF26, 0x80).spin();
    p.0
}

fn assert_in_range(samples: &[f32], label: &str) {
    for (i, &s) in samples.iter().enumerate() {
        assert!(
            s.is_finite() && (-1.0..=1.0).contains(&s),
            "{label}: sample {i} = {s}"
        );
    }
}

/// Run `frames` frames at `rate`, checking per-frame and cumulative counts.
/// Returns every sample produced.
fn run_at_rate(rom: &[u8], rate: u32, frames: usize) -> Vec<f32> {
    let mut gb = GameBoy::new(rom.to_vec()).unwrap();
    gb.set_audio_sample_rate(rate);
    assert_eq!(gb.audio_sample_rate(), rate);
    let (mut all, mut total_mc) = (Vec::new(), 0u64);
    for f in 0..frames {
        let before = gb.mcycles();
        gb.step_frame();
        let dm = gb.mcycles() - before;
        total_mc += dm;
        if f > 0 {
            // Literal per-frame rate: rate / 59.7275 stereo samples.
            // (the instruction crossing the frame edge shifts it by <= 6)
            assert!(
                dm.abs_diff(u64::from(M_CYCLES_PER_FRAME)) <= 6,
                "frame {f} took {dm} M-cycles"
            );
        }
        let s = gb.audio_samples();
        assert_eq!(s.len() % 2, 0, "rate {rate}: odd sample count");
        assert_in_range(&s, &format!("rate {rate} frame {f}"));
        let want = expected_pairs(rate, dm);
        let got = (s.len() / 2) as f64;
        assert!(
            (got - want).abs() <= 2.0,
            "rate {rate} frame {f}: {got} stereo samples, expected {want:.2}"
        );
        all.extend(s);
    }
    let want = expected_pairs(rate, total_mc);
    let got = (all.len() / 2) as f64;
    assert!(
        (got - want).abs() <= 2.0,
        "rate {rate}: {got} stereo samples over {frames} frames, expected {want:.2}"
    );
    all
}

#[test]
fn sample_count_tracks_rate_for_the_whole_range() {
    let rom = make_rom(0x8000, 0x00, 0, 0, &tone_program());
    let mut rates = vec![
        8_000, 8_001, 11_025, 16_000, 22_050, 32_000, 44_100, 44_101, 47_999, 48_000, 48_001,
        88_200, 96_000, 176_400, 191_999, 192_000,
    ];
    let mut rng = Rng::new(0xA0D10);
    rates.extend((0..24).map(|_| rng.range(8_000, 192_000) as u32));
    for rate in rates {
        run_at_rate(&rom, rate, 6);
    }
}

#[test]
fn tone_is_audible_and_bounded() {
    let rom = make_rom(0x8000, 0x00, 0, 0, &tone_program());
    let s = run_at_rate(&rom, 48_000, 30);
    let peak = s.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!(peak > 0.05, "tone ROM is silent (peak {peak})");
    assert!(peak <= 1.0);
    // It is a waveform, not a DC level.
    let (lo, hi) = s
        .iter()
        .fold((f32::MAX, f32::MIN), |(l, h), &v| (l.min(v), h.max(v)));
    assert!(hi - lo > 0.05, "no variation: {lo}..{hi}");
}

#[test]
fn random_apu_streams_stay_bounded() {
    let mut audible = 0;
    for seed in 0..24 * fuzz_scale() {
        let mut rng = Rng::new(0xAB5 + seed);
        let rom = make_rom(0x8000, 0x00, 0, 0, &noisy_apu_program(&mut rng));
        let rate = rng.range(8_000, 192_000) as u32;
        let s = run_at_rate(&rom, rate, 30);
        if s.iter().any(|&v| v != 0.0) {
            audible += 1;
        }
    }
    assert!(audible >= 8, "only {audible}/24 streams produced sound");
}

/// Out-of-range rates are clamped and the clamped rate is what is produced.
#[test]
fn out_of_range_rates_are_clamped() {
    let rom = make_rom(0x8000, 0x00, 0, 0, &tone_program());
    for (asked, want) in [
        (0u32, 8_000u32),
        (1, 8_000),
        (7_999, 8_000),
        (192_001, 192_000),
        (1_000_000, 192_000),
        (u32::MAX, 192_000),
    ] {
        let mut gb = GameBoy::new(rom.clone()).unwrap();
        gb.set_audio_sample_rate(asked);
        assert_eq!(gb.audio_sample_rate(), want, "asked {asked}");
        for _ in 0..3 {
            let before = gb.mcycles();
            gb.step_frame();
            let dm = gb.mcycles() - before;
            let s = gb.audio_samples();
            assert_in_range(&s, "clamped");
            assert!(((s.len() / 2) as f64 - expected_pairs(want, dm)).abs() <= 2.0);
        }
    }
}

/// Changing the rate every frame (the frontend does this on device changes).
#[test]
fn rate_changes_mid_run_stay_consistent() {
    let mut rng = Rng::new(0x3A7E);
    let rom = make_rom(0x8000, 0x00, 0, 0, &noisy_apu_program(&mut rng));
    let mut gb = GameBoy::new(rom).unwrap();
    for f in 0..80 {
        let rate = if rng.chance(1, 4) {
            rng.pick(&[8_000u32, 192_000, 0, u32::MAX])
        } else {
            rng.range(8_000, 192_000) as u32
        };
        gb.set_audio_sample_rate(rate);
        let rate = gb.audio_sample_rate();
        let before = gb.mcycles();
        gb.step_frame();
        let dm = gb.mcycles() - before;
        let s = gb.audio_samples();
        assert_in_range(&s, &format!("frame {f}"));
        assert!(
            ((s.len() / 2) as f64 - expected_pairs(rate, dm)).abs() <= 2.0,
            "frame {f} rate {rate}: {} samples for {dm} M-cycles",
            s.len() / 2
        );
    }
}

/// A frontend that stops draining must not make the core allocate without
/// bound: the buffer holds at most about one second of audio.
#[test]
fn undrained_audio_is_bounded() {
    let rom = make_rom(0x8000, 0x00, 0, 0, &tone_program());
    for rate in [8_000u32, 48_000, 192_000] {
        let mut gb = GameBoy::new(rom.clone()).unwrap();
        gb.set_audio_sample_rate(rate);
        // ~3.3 emulated seconds, never drained.
        for _ in 0..(3 * 1_048_576 / M_CYCLES_PER_FRAME as usize + 12) {
            gb.step_frame();
        }
        let s = gb.audio_samples();
        assert!(
            s.len() <= 2 * rate as usize,
            "rate {rate}: {} floats buffered",
            s.len()
        );
        assert_in_range(&s, "undrained");
        assert!(gb.audio_samples().is_empty(), "drain must empty the buffer");
    }
}

/// With the APU powered off (the post-boot default has it on, so switch it
/// off) the output is exact silence, not noise or NaN.
#[test]
fn powered_off_apu_is_silent() {
    let mut p = Program::default();
    p.write(0xFF26, 0x00).spin();
    let mut gb = GameBoy::new(make_rom(0x8000, 0x00, 0, 0, &p.0)).unwrap();
    gb.step_frame();
    gb.audio_samples();
    for _ in 0..10 {
        gb.step_frame();
        let s = gb.audio_samples();
        assert!(!s.is_empty());
        assert!(
            s.iter().all(|&v| v == 0.0),
            "non-silent output with APU off"
        );
    }
}
