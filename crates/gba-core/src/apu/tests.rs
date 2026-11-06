//! Register-level tests of the whole APU: registers, frame sequencer rates, the
//! mixer and the resampler. Per-channel behaviour is tested in each channel's
//! own module.

use super::*;

const CPS: u64 = CYCLES_PER_SECOND as u64;
/// One frame-sequencer step.
const STEP: u64 = SEQUENCER_CYCLES as u64;

fn on() -> Apu {
    let mut a = Apu::default();
    w(&mut a, SOUNDCNT_X, 0x80);
    w(&mut a, SOUNDCNT_L, 0xFF77);
    a
}

fn w(a: &mut Apu, off: u32, v: u16) {
    a.write_reg(off, v, 0xFFFF);
}

fn drain(a: &mut Apu) -> Vec<f32> {
    let mut v = vec![];
    a.drain_samples(&mut v);
    v
}

/// Run `cycles` and return the largest left and right sample.
fn peak(a: &mut Apu, cycles: u64) -> (f32, f32) {
    let t = a.now();
    a.advance_to(t + cycles);
    let s = drain(a);
    let max = |lane: usize| {
        s.iter()
            .skip(lane)
            .step_by(2)
            .copied()
            .fold(f32::MIN, f32::max)
    };
    (max(0), max(1))
}

/// Let the host sample in progress finish, so the next `peak` sees only the new
/// state, and discard what was produced.
fn settle(a: &mut Apu) {
    a.advance_to(a.now() + 2000);
    drain(a);
}

/// Channel 1 at volume 15, 50% duty, frequency 0: a 32768-cycle plateau high.
fn start_ch1(a: &mut Apu) {
    w(a, 0x62, 0xF080);
    w(a, 0x64, 0x8000);
}

#[test]
fn a_fresh_apu_is_silent_at_the_bias_centre() {
    let mut a = Apu::default();
    a.advance_to(CPS / 60);
    let s = drain(&mut a);
    assert!(!s.is_empty());
    assert!(s.iter().all(|&x| x == 0.0));
    assert_eq!(a.read_reg(SOUNDBIAS), 0x0200);
}

#[test]
fn master_off_resets_registers_and_channels_but_not_wave_ram() {
    let mut a = on();
    start_ch1(&mut a);
    w(&mut a, 0x70, 0x80); // channel 3 playback on
    w(&mut a, 0x90, 0x1234);
    w(&mut a, SOUNDCNT_H, 0x0304);
    assert_eq!(a.read_reg(0x84), 0x81, "master on, channel 1 playing");
    assert_eq!(a.read_reg(SOUNDCNT_L), 0xFF77);
    assert_eq!(a.read_reg(0x62), 0xF080 & 0xFFC0);

    w(&mut a, SOUNDCNT_X, 0x00);
    for off in (0x60..=0x80).step_by(2) {
        assert_eq!(a.read_reg(off), 0, "{off:#x} reads zero while off");
    }
    assert_eq!(a.read_reg(0x84), 0);
    assert_eq!(a.read_reg(SOUNDCNT_H), 0x0304, "SOUNDCNT_H stays");
    // Wave RAM (the non-playing bank) survives.
    assert_eq!(a.read_reg(0x90), 0x1234);

    // Writes are ignored while off ...
    w(&mut a, SOUNDCNT_L, 0xFFFF);
    w(&mut a, 0x62, 0xFFFF);
    assert_eq!(a.read_reg(SOUNDCNT_L), 0);
    assert_eq!(a.read_reg(0x62), 0);
    // ... and the channel is gone when it comes back.
    w(&mut a, SOUNDCNT_X, 0x80);
    assert_eq!(a.read_reg(0x84), 0x80);
    assert_eq!(a.read_reg(SOUNDCNT_L), 0);
}

#[test]
fn master_off_is_silent_but_the_bias_still_applies() {
    let mut a = on();
    start_ch1(&mut a);
    a.write_fifo(false, 0x7F7F_7F7F);
    w(&mut a, SOUNDCNT_H, 0x0304);
    a.on_timer_overflow(0);
    let (l, _) = peak(&mut a, CPS / 100);
    assert!(l > 0.5);
    w(&mut a, SOUNDCNT_X, 0);
    settle(&mut a);
    assert_eq!(peak(&mut a, CPS / 100), (0.0, 0.0));
    w(&mut a, SOUNDBIAS, 0);
    settle(&mut a);
    assert_eq!(
        peak(&mut a, CPS / 100),
        (-1.0, -1.0),
        "bias 0 is the bottom"
    );
}

#[test]
fn only_readable_bits_show() {
    let mut a = on();
    w(&mut a, 0x64, 0xFFFF);
    assert_eq!(a.read_reg(0x64), 0x4000);
    w(&mut a, 0x60, 0xFFFF);
    assert_eq!(a.read_reg(0x60), 0x007F);
    w(&mut a, 0x70, 0xFFFF);
    assert_eq!(a.read_reg(0x70), 0x00E0);
    w(&mut a, SOUNDCNT_H, 0xFFFF);
    assert_eq!(a.read_reg(SOUNDCNT_H), 0x770F, "reset bits read 0");
    w(&mut a, SOUNDBIAS, 0xFFFF);
    assert_eq!(a.read_reg(SOUNDBIAS), 0xC3FE);
    w(&mut a, 0x86, 0xFFFF);
    assert_eq!(a.read_reg(0x86), 0);
    // SOUNDCNT_X: only bit 7 is writable; flags are read-only.
    w(&mut a, SOUNDCNT_X, 0x8F);
    assert_eq!(a.read_reg(SOUNDCNT_X) & 0x0F, 0, "no channel started");
}

#[test]
fn byte_lane_writes_reach_the_channels() {
    let mut a = on();
    a.write_reg(0x62, 0xF000, 0xFF00); // envelope byte only
    a.write_reg(0x64, 0x0000, 0x00FF); // frequency low
    a.write_reg(0x64, 0x8000, 0xFF00); // restart lane
    assert_eq!(a.read_reg(0x84) & 1, 1);
    assert_eq!(a.ch1.env.nr2, 0xF0);
}

#[test]
fn channel_status_flags_follow_length_expiry() {
    let mut a = on();
    w(&mut a, 0x62, 0xF000 | 63); // length 1 clock
    w(&mut a, 0x64, 0xC000); // restart with the length flag
    assert_eq!(a.read_reg(0x84) & 1, 1);
    a.advance_to(STEP + 10);
    assert_eq!(a.read_reg(0x84) & 1, 0);
}

#[test]
fn length_clocks_at_256_hz() {
    let mut a = on();
    // Two clocks left: they fall on sequencer steps 0 and 2.
    w(&mut a, 0x62, 0xF000 | 62);
    w(&mut a, 0x64, 0xC000);
    a.advance_to(2 * STEP);
    assert!(a.ch1.enabled, "step 1 does not clock length");
    a.advance_to(3 * STEP - 1);
    assert!(a.ch1.enabled);
    a.advance_to(3 * STEP);
    assert!(
        !a.ch1.enabled,
        "expired on the second even step, 256 Hz apart"
    );
    assert_eq!(2 * STEP, CPS / 256);
}

/// The APU hands each PSG control write the parity of the sequencer step that
/// runs next, so the length quirks fire on an odd one (deep-review #73).
#[test]
fn length_quirks_follow_the_sequencer_step() {
    let mut a = on();
    a.advance_to(STEP + 10);
    assert_eq!(a.seq_step, 1, "the next step is odd");
    w(&mut a, 0x62, 0xF000 | 62); // length counter 2
    w(&mut a, 0x64, 0x4000); // length on, no restart
    assert_eq!(a.ch1.length.counter, 1, "the enable clocked once");
    // Channel 4 gets the same parity.
    w(&mut a, 0x78, 62);
    w(&mut a, 0x7C, 0x4000);
    assert_eq!(a.ch4.length.counter, 1);
    // On an even step the same writes add nothing.
    let mut a = on();
    assert_eq!(a.seq_step, 0);
    w(&mut a, 0x62, 0xF000 | 62);
    w(&mut a, 0x64, 0x4000);
    assert_eq!(a.ch1.length.counter, 2);
}

/// The APU-level `load_state` refusals, each on its own field (deep-review #74);
/// the square channels' own are tested in their module.
#[test]
fn a_state_with_an_impossible_apu_field_is_refused() {
    let mut ok = on();
    start_ch1(&mut ok);
    ok.advance_to(2 * STEP + 5);
    let load = |a: &Apu| {
        let mut w = StateWriter::new();
        a.save_state(&mut w);
        let bytes = w.into_bytes();
        Apu::default().load_state(&mut StateReader::new(&bytes), a.now())
    };
    assert!(load(&ok).is_ok());
    let cases: [(&str, fn(&mut Apu)); 8] = [
        ("frame sequencer step", |a| a.seq_step = 8),
        ("frame sequencer timer", |a| a.seq_timer = 0),
        ("mix level", |a| a.cur_l = -1),
        ("mix level", |a| a.cur_r = MIX_MAX + 1),
        ("resampler cycle count", |a| a.mix_n = u32::MAX),
        ("resampler sums", |a| a.mix_l = -1),
        ("resampler sums", |a| a.mix_r = i64::MAX),
        ("resampler phase", |a| a.sample_acc = u64::MAX),
    ];
    for (name, break_it) in cases {
        let mut bad = ok.clone();
        break_it(&mut bad);
        assert_eq!(load(&bad), Err(StateError::Invalid(name)), "{name}");
    }
}

#[test]
fn envelope_steps_at_64_hz() {
    let mut a = on();
    w(&mut a, 0x62, 0xF100); // volume 15, decrease, step time 1
    w(&mut a, 0x64, 0x8000);
    a.advance_to(8 * STEP - 1);
    assert_eq!(a.ch1.env.volume, 15);
    a.advance_to(8 * STEP);
    assert_eq!(
        a.ch1.env.volume, 14,
        "first envelope step at sequencer step 7"
    );
    a.advance_to(16 * STEP);
    assert_eq!(a.ch1.env.volume, 13);
    assert_eq!(8 * STEP, CPS / 64);
}

#[test]
fn sweep_steps_at_128_hz() {
    let mut a = on();
    w(&mut a, 0x60, 0x0011); // time 1, increase, shift 1
    w(&mut a, 0x62, 0xF000);
    w(&mut a, 0x64, 0x8100);
    a.advance_to(3 * STEP - 1);
    assert_eq!(a.ch1.freq, 0x100);
    a.advance_to(3 * STEP);
    assert_eq!(a.ch1.freq, 0x180, "sequencer step 2");
    a.advance_to(7 * STEP);
    assert_eq!(a.ch1.freq, 0x240, "step 6, 128 Hz apart");
    assert_eq!(4 * STEP, CPS / 128);
}

#[test]
fn psg_ratio_scales_25_50_100() {
    let mut pk = vec![];
    for ratio in 0..3 {
        let mut a = on();
        w(&mut a, SOUNDCNT_H, ratio);
        start_ch1(&mut a);
        pk.push(peak(&mut a, STEP).0);
    }
    assert!(pk[0] > 0.0);
    assert!((pk[1] / pk[0] - 2.0).abs() < 1e-5, "{pk:?}");
    assert!((pk[2] / pk[1] - 2.0).abs() < 1e-5, "{pk:?}");
    // The prohibited value is mute.
    let mut a = on();
    w(&mut a, SOUNDCNT_H, 3);
    start_ch1(&mut a);
    assert_eq!(peak(&mut a, STEP), (0.0, 0.0));
}

#[test]
fn psg_master_volume_and_enables_are_per_side() {
    // Channel 1 high for a whole step under the given SOUNDCNT_L.
    let sides = |cnt_l: u16| {
        let mut a = on();
        w(&mut a, SOUNDCNT_H, 2);
        w(&mut a, SOUNDCNT_L, cnt_l);
        start_ch1(&mut a);
        peak(&mut a, STEP)
    };
    // Both sides, volume 8 on the left and 4 on the right.
    let (l, r) = sides(0x1000 | 0x0100 | 0x0070 | 0x0003);
    assert!((l / r - 2.0).abs() < 1e-5, "volume 8 vs 4: {l} {r}");
    let (l, r) = sides(0x1000 | 0x0077);
    assert!(l > 0.0);
    assert_eq!(r, 0.0, "channel 1 not enabled on the right");
    let (l, r) = sides(0x0100 | 0x0077);
    assert_eq!((l, r > 0.0), (0.0, true), "right only");
    assert_eq!(sides(0x0077), (0.0, 0.0), "no channel enabled");
    // Another channel's bit does not enable channel 1.
    assert_eq!(sides(0x2200 | 0x0077), (0.0, 0.0));
}

#[test]
fn psg_channels_sum() {
    let mut a = on();
    w(&mut a, SOUNDCNT_H, 2);
    start_ch1(&mut a);
    let one = peak(&mut a, STEP).0;
    let mut a = on();
    w(&mut a, SOUNDCNT_H, 2);
    start_ch1(&mut a);
    w(&mut a, 0x68, 0xF080);
    w(&mut a, 0x6C, 0x8000);
    let two = peak(&mut a, STEP).0;
    assert!((two / one - 2.0).abs() < 1e-5, "{one} {two}");
}

fn fifo_a(a: &mut Apu, cnt_h: u16, word: u32) {
    w(a, SOUNDCNT_H, cnt_h);
    a.write_fifo(false, word);
    a.on_timer_overflow(0);
}

#[test]
fn direct_sound_level_and_volume() {
    // 64 at 100%: 64 * 16 units over a 2048-unit half range = 0.5.
    let mut a = on();
    fifo_a(&mut a, 0x0304, 0x4040_4040);
    assert_eq!(peak(&mut a, 1000), (0.5, 0.5));
    // 50%: 0.25.
    let mut a = on();
    fifo_a(&mut a, 0x0300, 0x4040_4040);
    assert_eq!(peak(&mut a, 1000), (0.25, 0.25));
    // Negative samples go below the centre; -128 at 100% is the bottom.
    let mut a = on();
    fifo_a(&mut a, 0x0304, 0x8080_8080);
    let s = {
        a.advance_to(a.now() + 1000);
        drain(&mut a)
    };
    assert_eq!(*s.last().unwrap(), -1.0);
}

#[test]
fn direct_sound_enables_per_side_and_fifo_b() {
    let mut a = on();
    fifo_a(&mut a, 0x0104, 0x4040_4040); // A right only
    assert_eq!(peak(&mut a, 1000), (0.0, 0.5));
    let mut a = on();
    w(&mut a, SOUNDCNT_H, 0x2000 | 0x0008); // B left, 100%
    a.write_fifo(true, 0x2020_2020);
    a.on_timer_overflow(0);
    assert_eq!(peak(&mut a, 1000), (0.25, 0.0));
}

#[test]
fn direct_sound_streams_one_sample_per_timer_overflow() {
    let mut a = on();
    w(&mut a, SOUNDCNT_H, 0x0304);
    a.write_fifo(false, 0x1008_0402);
    let mut got = vec![];
    for _ in 0..4 {
        a.on_timer_overflow(0);
        a.advance_to(a.now() + 5_000);
        got.push(*drain(&mut a).last().unwrap());
    }
    // 16 units per sample step over 2048.
    assert_eq!(got, [2.0 / 128.0, 4.0 / 128.0, 8.0 / 128.0, 16.0 / 128.0]);
}

#[test]
fn timer_select_decides_which_timer_pops() {
    let mut a = on();
    w(&mut a, SOUNDCNT_H, 0x0304 | 0x0400); // A on timer 1
    a.write_fifo(false, 0x4040_4040);
    a.on_timer_overflow(0);
    assert_eq!(peak(&mut a, 1000), (0.0, 0.0), "timer 0 does not clock A");
    a.on_timer_overflow(1);
    assert_eq!(peak(&mut a, 1000), (0.5, 0.5));
}

#[test]
fn refill_is_requested_at_16_bytes() {
    let mut a = on();
    w(&mut a, SOUNDCNT_H, 0x0304 | 0x4000); // A timer 0, B timer 1
    for _ in 0..8 {
        a.write_fifo(false, 0x0101_0101);
        a.write_fifo(true, 0x0101_0101);
    }
    // 32 bytes: the first pops leave 31..17.
    for _ in 0..15 {
        assert_eq!(a.on_timer_overflow(0), FifoRequest::default());
    }
    assert_eq!(
        a.on_timer_overflow(0),
        FifoRequest { a: true, b: false },
        "16 left"
    );
    assert_eq!(a.on_timer_overflow(1), FifoRequest { a: false, b: false });
    // A refill keeps being asked for until the FIFO is topped up.
    assert!(a.on_timer_overflow(0).a);
    for _ in 0..4 {
        a.write_fifo(false, 0);
    }
    assert!(!a.on_timer_overflow(0).a);
    // Timers 2 and 3 never reach the APU, and an unselected timer asks for nothing.
    assert!(!a.on_timer_overflow(1).a);
}

#[test]
fn fifo_reset_bits_empty_the_fifos() {
    let mut a = on();
    w(&mut a, SOUNDCNT_H, 0x0304);
    a.write_fifo(false, 0x4040_4040);
    a.on_timer_overflow(0);
    assert!(peak(&mut a, 1000).0 > 0.0);
    w(&mut a, SOUNDCNT_H, 0x0304 | 0x0800); // reset A
    assert_eq!(
        a.read_reg(SOUNDCNT_H),
        0x0304,
        "the reset bit does not stick"
    );
    settle(&mut a);
    assert_eq!(peak(&mut a, 1000), (0.0, 0.0), "output silenced");
    // Empty: the next pops request a refill and stay silent.
    assert!(a.on_timer_overflow(0).a);
    assert_eq!(peak(&mut a, 1000), (0.0, 0.0));
}

#[test]
fn soundbias_clamps_to_ten_bits() {
    let mut a = on();
    w(&mut a, SOUNDBIAS, 0x03FE);
    fifo_a(&mut a, 0x0304, 0x7F7F_7F7F);
    let (l, _) = peak(&mut a, 1000);
    assert_eq!(l, 4095.0 / 2048.0 - 1.0, "top of the 10-bit range");
    // Both FIFOs plus a high bias would overflow 10 bits without the clamp.
    w(&mut a, SOUNDCNT_H, 0x0304 | 0x3300);
    a.write_fifo(true, 0x7F7F_7F7F);
    a.on_timer_overflow(0);
    assert!(peak(&mut a, 1000).0 <= 1.0);
    // Bias 0 and a negative sample clamp at the bottom.
    let mut a = on();
    w(&mut a, SOUNDBIAS, 0);
    fifo_a(&mut a, 0x0304, 0x8080_8080);
    a.advance_to(a.now() + 1000);
    assert_eq!(*drain(&mut a).last().unwrap(), -1.0);
}

#[test]
fn the_bias_level_shifts_the_output() {
    let mut a = on();
    w(&mut a, SOUNDBIAS, 0x0300); // 0x300: +0x100 over the centre
    a.advance_to(1000);
    assert_eq!(*drain(&mut a).last().unwrap(), 0.5);
}

#[test]
fn drains_exactly_the_samples_for_the_elapsed_time() {
    for rate in [8_000u32, 32_768, 44_100, 48_000, 96_000, 192_000] {
        let mut a = Apu::default();
        a.set_sample_rate(rate);
        let mut total = 0;
        let mut t = 0u64;
        // Irregular chunks, drained as we go.
        for i in 0..2000u64 {
            t += 1 + (i * 7919) % 3001;
            a.advance_to(t);
            total += drain(&mut a).len();
        }
        assert_eq!(total as u64, 2 * (u64::from(rate) * t / CPS), "rate {rate}");
    }
}

#[test]
fn sixty_frames_at_48k_match_the_emulated_time() {
    let mut a = Apu::default();
    let frame = u64::from(crate::CYCLES_PER_FRAME);
    let mut total = 0;
    for f in 1..=60u64 {
        a.advance_to(f * frame);
        total += drain(&mut a).len();
    }
    let expected = 48_000.0 * 60.0 / crate::FRAMES_PER_SECOND;
    assert!(
        (total as f64 / 2.0 - expected).abs() <= 1.0,
        "{} vs {expected}",
        total / 2
    );
}

#[test]
fn chunking_does_not_change_the_audio() {
    let run = |chunk: u64| {
        let mut a = on();
        w(&mut a, SOUNDCNT_H, 2);
        start_ch1(&mut a);
        w(&mut a, 0x78, 0xF100);
        w(&mut a, 0x7C, 0x8030);
        let mut t = 0;
        while t < 600_000 {
            t += chunk;
            a.advance_to(t);
        }
        a.advance_to(600_000);
        drain(&mut a)
    };
    let a = run(600_000);
    assert_eq!(a, run(512));
    assert_eq!(a, run(97));
}

#[test]
fn a_changed_rate_keeps_counting_exactly() {
    let mut a = Apu::default();
    a.advance_to(CPS / 4);
    let n1 = drain(&mut a).len() as u64 / 2;
    a.set_sample_rate(24_000);
    a.advance_to(CPS / 2);
    let n2 = drain(&mut a).len() as u64 / 2;
    assert_eq!(n1, 48_000 / 4);
    assert!((n2 as i64 - 24_000 / 4).abs() <= 1, "{n2}");
}

#[test]
fn an_undrained_buffer_is_capped() {
    let mut a = Apu::default();
    a.advance_to(5 * CPS);
    assert_eq!(drain(&mut a).len(), 2 * 48_000);
}

/// Same policy as gb-core: when full, the newest samples are dropped.
#[test]
fn a_full_buffer_keeps_the_oldest_samples() {
    let mut a = Apu::default();
    a.advance_to(CPS / 2); // silence at the bias centre
    w(&mut a, SOUNDBIAS, 0x0300); // from now on the level is 0.5
    a.advance_to(5 * CPS);
    let s = drain(&mut a);
    assert_eq!(s.len(), 2 * 48_000);
    assert_eq!(s[0], 0.0);
    assert_eq!(s[2 * 24_000 - 2], 0.0, "the first half second is kept");
    assert_eq!(
        *s.last().unwrap(),
        0.5,
        "the cap falls inside the later audio"
    );
    // Draining makes room again.
    a.advance_to(6 * CPS);
    assert!(!drain(&mut a).is_empty());
}

#[test]
fn wave_ram_cpu_bank_is_the_one_not_playing() {
    let mut a = on();
    w(&mut a, 0x90, 0xAAAA); // bank 1 (bank 0 is selected: CPU sees bank 1)
    w(&mut a, 0x70, 0x40); // select bank 1: CPU now sees bank 0
    assert_eq!(a.read_reg(0x90), 0);
    w(&mut a, 0x92, 0xBBBB);
    w(&mut a, 0x70, 0x00);
    assert_eq!(a.read_reg(0x90), 0xAAAA);
    assert_eq!(a.read_reg(0x92), 0);
    a.write_reg(0x9E, 0x1200, 0xFF00);
    assert_eq!(a.read_reg(0x9E), 0x1200);
}

#[test]
fn wave_channel_plays_through_the_mixer() {
    let mut a = on();
    w(&mut a, SOUNDCNT_H, 2);
    w(&mut a, 0x90, 0xFFFF); // bank 1 filled; bank 0 plays
    w(&mut a, 0x70, 0x40); // bank 1 plays, CPU fills bank 0
    w(&mut a, 0x90, 0x0000);
    w(&mut a, 0x70, 0x80); // bank 0 plays, playback on
    w(&mut a, 0x72, 0x2000); // 100%
    w(&mut a, 0x74, 0x8000);
    assert_eq!(a.read_reg(0x84) & 4, 4);
    let (l, _) = peak(&mut a, STEP);
    assert_eq!(l, 0.0, "bank 0 starts with zero digits");
    w(&mut a, 0x70, 0xC0); // switch to bank 1 live
    w(&mut a, 0x74, 0x8000);
    let (l, _) = peak(&mut a, STEP);
    assert_eq!(l, 15.0 * 8.0 * 4.0 / 2048.0);
}

#[test]
fn noise_channel_is_audible() {
    let mut a = on();
    w(&mut a, SOUNDCNT_H, 2);
    w(&mut a, 0x78, 0xF000);
    w(&mut a, 0x7C, 0x8030);
    let (l, _) = peak(&mut a, 4 * STEP);
    assert!(l > 0.0);
}
