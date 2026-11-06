//! The APU behind the bus: register routing, the FIFO timer/DMA path, resampled
//! length, determinism and the pinned audio hash.

mod apu_common;
use apu_common::*;
use gba_core::bus::CpuBus;
use gba_core::{CYCLES_PER_FRAME, CYCLES_PER_SECOND, Gba};

fn rd(bus: &mut gba_core::bus::Bus, off: u32) -> u16 {
    bus.read16(0x0400_0000 + off, N)
}

fn wr(bus: &mut gba_core::bus::Bus, off: u32, v: u16) {
    bus.write16(0x0400_0000 + off, v, N);
}

#[test]
fn sound_registers_are_routed_to_the_apu() {
    let mut b = new_bus();
    assert_eq!(rd(&mut b, 0x88), 0x0200, "SOUNDBIAS default");
    assert_eq!(rd(&mut b, 0x84), 0);
    wr(&mut b, 0x84, 0x80);
    wr(&mut b, 0x80, 0xFFFF);
    assert_eq!(rd(&mut b, 0x80), 0xFF77);
    // A byte write reaches one lane only.
    b.write8(0x0400_0081, 0x00, N);
    assert_eq!(rd(&mut b, 0x80), 0x0077);
    // 32-bit write: SOUNDCNT_L (low) and SOUNDCNT_H (high).
    b.write32(0x0400_0080, 0x0304_1234, N);
    assert_eq!(rd(&mut b, 0x80), 0x1234 & 0xFF77);
    assert_eq!(rd(&mut b, 0x82), 0x0304);
    // A restart: SOUND1CNT_H (32-bit write with SOUND1CNT_L), then SOUND1CNT_X.
    b.write32(0x0400_0060, 0xF000 << 16, N);
    b.write32(0x0400_0064, 0x8000, N);
    assert_eq!(rd(&mut b, 0x84) & 1, 1, "channel 1 running");
    // The FIFOs are write-only.
    b.write32(FIFO_A, 0x7F7F_7F7F, N);
    assert_eq!(b.read32(FIFO_A, N), 0);
}

#[test]
fn a_fifo_word_is_pushed_once_per_32_bit_write() {
    let mut b = new_bus();
    wr(&mut b, 0x84, 0x80);
    wr(&mut b, 0x82, 0x0304);
    b.write32(FIFO_A, 0x0000_0040, N);
    for _ in 0..4 {
        b.apu.on_timer_overflow(0);
    }
    // Four samples were queued: the fifth pop is the first of an empty FIFO.
    assert!(b.apu.on_timer_overflow(0).a);
    // Halfword writes, low then high, make one word too.
    b.write16(FIFO_B, 0x0101, N);
    b.write16(FIFO_B + 2, 0x0101, N);
}

#[test]
fn timer_overflows_pop_samples_and_trigger_the_fifo_dma() {
    let mut b = new_bus();
    for i in 0..64 {
        b.write32(IWRAM + 4 * i, 0x4040_4040, N);
    }
    wr(&mut b, 0x84, 0x80);
    wr(&mut b, 0x82, 0x0304); // A on both sides, 100%, timer 0
    b.write32(FIFO_A, 0, N); // primed with four silent samples
    // DMA 1: special timing, repeat, fixed destination = FIFO A.
    b.write32(0x0400_00BC, IWRAM, N);
    b.write32(0x0400_00C0, FIFO_A, N);
    b.write32(
        0x0400_00C4,
        4 | u32::from(0x8000u16 | 3 << 12 | 1 << 9 | 2 << 5) << 16,
        N,
    );
    // Timer 0 overflows every 512 cycles.
    b.write32(0x0400_0100, 0xFE00 | 0x80 << 16, N);
    run(&mut b, 100_000);
    b.apu.advance_to(b.sched.now());
    let mut s = vec![];
    b.apu.drain_samples(&mut s);
    // Silence for the primed samples, then 64/128 x 100% = 0.5 from the DMA.
    assert_eq!(s[0], 0.0);
    let loud = s.iter().filter(|&&x| x == 0.5).count();
    assert!(loud > 100, "DMA fed the FIFO: {loud} loud samples");
    assert_ne!(rd(&mut b, 0xC6) & 0x8000, 0, "DMA 1 stays enabled");
}

#[test]
fn the_scene_is_audible_on_both_sides() {
    let (s, _) = scene_audio(10);
    let peak = |lane: usize| {
        s.iter()
            .skip(lane)
            .step_by(2)
            .fold(0.0f32, |m, x| m.max(x.abs()))
    };
    assert!(peak(0) > 0.05 && peak(1) > 0.05, "{} {}", peak(0), peak(1));
    assert!(s.iter().all(|x| (-1.0..=1.0).contains(x)));
}

#[test]
fn identical_machines_produce_identical_audio() {
    let (a, ca) = scene_audio(20);
    let (b, cb) = scene_audio(20);
    assert_eq!(ca, cb);
    assert_eq!(a.len(), b.len());
    assert!(
        a.iter().zip(&b).all(|(x, y)| x.to_bits() == y.to_bits()),
        "bit-identical"
    );
}

/// Pinned audio of the scene (`start_scene`, 48 kHz, 30 frames from power-on).
/// A change here means the sound output changed: review it, then update.
const SCENE_HASH_30_FRAMES: u64 = 0xB283_7FCB_6C8B_7775;

#[test]
fn the_scene_audio_hash_is_pinned() {
    let (s, cycles) = scene_audio(30);
    assert_eq!(
        s.len() as u64 / 2,
        48_000 * cycles / u64::from(CYCLES_PER_SECOND)
    );
    assert_eq!(hash_audio(&s), SCENE_HASH_30_FRAMES, "audio hash changed");
}

#[test]
fn audio_does_not_depend_on_how_often_it_is_drained() {
    let (mut a, mut b) = (new_bus(), new_bus());
    start_scene(&mut a);
    start_scene(&mut b);
    let (mut sa, mut sb) = (vec![], vec![]);
    for _ in 0..6 {
        run(&mut a, 70_000);
        run(&mut b, 70_000);
        // `a` syncs and drains every step (as a frontend does), `b` only at the end.
        a.apu.advance_to(a.sched.now());
        a.apu.drain_samples(&mut sa);
    }
    b.apu.advance_to(b.sched.now());
    b.apu.drain_samples(&mut sb);
    assert_eq!(sa.len(), sb.len());
    assert!(sa.iter().zip(&sb).all(|(x, y)| x.to_bits() == y.to_bits()));
}

#[test]
fn gba_drain_returns_exactly_the_samples_for_the_elapsed_time() {
    let mut gba = Gba::new(vec![0; 0x200], None).unwrap();
    let mut sink = vec![];
    // Start from a frame boundary: the first frame stops at line 160.
    gba.step_frame();
    gba.drain_audio_into(&mut sink);
    let t0 = gba.cycles();
    let mut total = 0usize;
    for _ in 0..60 {
        gba.step_frame();
        sink.clear();
        gba.drain_audio_into(&mut sink);
        total += sink.len() / 2;
    }
    let cycles = gba.cycles() - t0;
    // step_frame stops after the instruction that crosses VBlank, so a real
    // CPU overshoots each frame end by a few cycles (DECISIONS #45). The
    // sample count below is checked against the cycles that really elapsed.
    let frames = 60 * u64::from(CYCLES_PER_FRAME);
    assert!(
        (frames..frames + 64).contains(&cycles),
        "{cycles} cycles for 60 frames"
    );
    // 60 frames of 48 kHz audio: 48 000 x 60 / 59.7275 = 48 218.9 samples.
    assert!((48_218..=48_219).contains(&total), "{total}");
    let cps = u64::from(CYCLES_PER_SECOND);
    assert_eq!(
        total as u64,
        48_000 * (t0 + cycles) / cps - 48_000 * t0 / cps,
        "exact for the elapsed time"
    );
    assert_eq!(gba.audio_sample_rate(), 48_000);
}

#[test]
fn gba_audio_follows_the_set_rate() {
    let mut gba = Gba::new(vec![0; 0x200], None).unwrap();
    gba.set_audio_sample_rate(44_100);
    gba.step_frame();
    let mut s = vec![];
    gba.drain_audio_into(&mut s);
    let want = 44_100 * gba.cycles() / u64::from(CYCLES_PER_SECOND);
    assert_eq!(s.len() as u64 / 2, want);
}

#[test]
fn the_pump_keeps_the_apu_within_one_interval_of_the_clock() {
    let mut b = new_bus();
    run(&mut b, 100_000);
    let lag = b.sched.now() - b.apu.now();
    assert!(lag <= gba_core::apu::PUMP_CYCLES + 48, "lag {lag}");
}

/// code-review A2#1: reads see the APU at the current cycle, not the last pump.
#[test]
fn sound_register_reads_see_the_current_cycle() {
    let mut b = new_bus();
    wr(&mut b, 0x84, 0x80);
    wr(&mut b, 0x62, 0xF000 | 63); // length 1
    wr(&mut b, 0x64, 0xC000); // restart with the length flag
    assert_eq!(rd(&mut b, 0x84) & 1, 1);
    // The first length clock is 32768 cycles after sound was enabled. Move the
    // clock past it without dispatching events, so no pump syncs the APU.
    b.idle(32_768 + 100);
    assert!(
        b.apu.now() < b.sched.now() - 32_000,
        "the APU lags the clock"
    );
    assert_eq!(rd(&mut b, 0x84) & 1, 0, "already expired");
}

/// code-review A2#2: a rate change applies from now on, not retroactively.
#[test]
fn a_rate_change_through_the_facade_counts_each_stretch_at_its_rate() {
    let mut gba = Gba::new(vec![0; 0x200], None).unwrap();
    gba.step_frame();
    gba.step_frame();
    let t1 = gba.cycles();
    gba.set_audio_sample_rate(24_000);
    gba.step_frame();
    gba.step_frame();
    let t2 = gba.cycles() - t1;
    let mut s = vec![];
    gba.drain_audio_into(&mut s);
    let cps = u64::from(CYCLES_PER_SECOND);
    assert_eq!(s.len() as u64 / 2, (48_000 * t1 + 24_000 * t2) / cps);
    assert_eq!(gba.audio_sample_rate(), 24_000);
}
