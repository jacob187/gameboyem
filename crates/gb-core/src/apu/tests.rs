use super::*;

/// M-cycles between frame-sequencer ticks: DIV bit 4 falls every 8192 T-cycles.
const SEQ_PERIOD: u32 = 2048;

/// An APU that was powered off and on again: no post-boot channel state, and
/// the frame sequencer at step 0.
fn fresh() -> Apu {
    let mut apu = Apu::new();
    apu.write(0xFF26, 0x00);
    apu.write(0xFF26, 0x80);
    apu
}

/// Drives an APU with a synthetic system counter advancing 4 T-cycles per call.
struct Rig {
    apu: Apu,
    div: u16,
}

impl Rig {
    fn new() -> Self {
        Self {
            apu: fresh(),
            div: 0,
        }
    }

    fn run(&mut self, mcycles: u32) {
        for _ in 0..mcycles {
            self.div = self.div.wrapping_add(4);
            self.apu.tick_mcycle(self.div);
        }
    }

    /// Run until exactly `n` more frame-sequencer ticks have happened.
    fn seq_ticks(&mut self, n: u32) {
        self.run(n * SEQ_PERIOD);
    }

    fn w(&mut self, addr: u16, v: u8) {
        self.apu.write(addr, v);
    }

    fn status(&self) -> u8 {
        self.apu.read(0xFF26) & 0x0F
    }
}

#[test]
fn sequencer_steps_on_div_bit4_falling_edge() {
    let mut rig = Rig::new();
    // Counter goes 4, 8, ... 0x1FFC: bit 12 rises at 0x1000, has not fallen yet.
    rig.run(SEQ_PERIOD - 1);
    assert_eq!(rig.apu.seq_step, 0);
    rig.run(1); // counter = 0x2000, bit 12 falls
    assert_eq!(rig.apu.seq_step, 1);
    rig.seq_ticks(1);
    assert_eq!(rig.apu.seq_step, 2);
    rig.seq_ticks(6);
    assert_eq!(rig.apu.seq_step, 0, "wraps after 8 steps");
}

#[test]
fn sequencer_ticks_when_div_is_reset_with_bit_set() {
    let mut apu = fresh();
    apu.tick_mcycle(0x1000);
    assert_eq!(apu.seq_step, 0);
    apu.tick_mcycle(0x0000); // DIV write clears the counter: falling edge
    assert_eq!(apu.seq_step, 1);
}

#[test]
fn first_div_apu_event_is_skipped_when_powering_on_with_div_bit_high() {
    let mut apu = Apu::new();
    apu.tick_mcycle(0x1000); // DIV bit 4 high
    apu.write(0xFF26, 0x00);
    apu.tick_mcycle(0x1000);
    apu.write(0xFF26, 0x80);
    apu.tick_mcycle(0x0000); // first falling edge: lost
    assert_eq!(apu.seq_step, 0);
    apu.tick_mcycle(0x1000);
    apu.tick_mcycle(0x0000); // second edge runs step 0
    assert_eq!(apu.seq_step, 1);
}

#[test]
fn no_div_apu_event_is_skipped_when_powering_on_with_div_bit_low() {
    let mut apu = Apu::new();
    apu.tick_mcycle(0x0000);
    apu.write(0xFF26, 0x00);
    apu.write(0xFF26, 0x80);
    apu.tick_mcycle(0x1000);
    apu.tick_mcycle(0x0000);
    assert_eq!(apu.seq_step, 1);
}

#[test]
fn skipped_div_apu_event_does_not_survive_a_power_cycle() {
    let mut apu = Apu::new();
    apu.tick_mcycle(0x1000);
    apu.write(0xFF26, 0x00);
    apu.write(0xFF26, 0x80); // armed
    apu.write(0xFF26, 0x00);
    apu.tick_mcycle(0x0000);
    apu.write(0xFF26, 0x80); // bit low now: nothing to skip
    apu.tick_mcycle(0x1000);
    apu.tick_mcycle(0x0000);
    assert_eq!(apu.seq_step, 1);
}

#[test]
fn sequencer_is_frozen_while_powered_off() {
    let mut apu = fresh();
    apu.write(0xFF26, 0x00);
    apu.tick_mcycle(0x1000);
    apu.tick_mcycle(0x0000);
    apu.write(0xFF26, 0x80);
    assert_eq!(apu.seq_step, 0);
}

#[test]
fn length_expires_on_even_steps_for_pulse_channels() {
    for (nr1, nr2, nr4, bit) in [
        (0xFF11u16, 0xFF12u16, 0xFF14u16, 0x01u8),
        (0xFF16, 0xFF17, 0xFF19, 0x02),
    ] {
        let mut rig = Rig::new();
        rig.w(nr2, 0xF0);
        rig.w(nr1, 0x3E); // length counter = 64 - 62 = 2
        rig.w(nr4, 0xC0); // trigger + length enable
        assert_eq!(rig.status() & bit, bit);
        rig.seq_ticks(2); // steps 0 and 1: one length clock
        assert_eq!(rig.status() & bit, bit);
        rig.seq_ticks(1); // step 2: second length clock
        assert_eq!(rig.status() & bit, 0, "channel {bit} should be off");
    }
}

#[test]
fn length_disabled_does_not_expire() {
    let mut rig = Rig::new();
    rig.w(0xFF12, 0xF0);
    rig.w(0xFF11, 0x3F);
    rig.w(0xFF14, 0x80);
    rig.seq_ticks(16);
    assert_eq!(rig.status() & 1, 1);
}

#[test]
fn length_extra_clock_when_enabled_before_odd_step() {
    let mut rig = Rig::new();
    rig.w(0xFF12, 0xF0);
    rig.w(0xFF11, 0x3F); // counter = 1
    rig.w(0xFF14, 0x80); // trigger, length disabled
    rig.seq_ticks(1); // next step is now 1 (odd): no length clock
    rig.w(0xFF14, 0x40); // enabling length clocks once: 1 -> 0, channel off
    assert_eq!(rig.status() & 1, 0);
}

#[test]
fn trigger_with_zero_length_reloads_and_dac_gates_trigger() {
    let mut rig = Rig::new();
    rig.w(0xFF14, 0x80); // DAC off (NR12 = 0): trigger must not enable
    assert_eq!(rig.status() & 1, 0);
    rig.w(0xFF12, 0x08); // DAC on (bit 3)
    rig.w(0xFF11, 0x3F); // counter = 1
    rig.w(0xFF14, 0xC0);
    rig.seq_ticks(1); // step 0 clocks length to 0: channel off
    assert_eq!(rig.apu.ch1.length.counter, 0);
    assert_eq!(rig.status() & 1, 0);
    rig.w(0xFF14, 0x80); // retrigger with length disabled
    assert_eq!(rig.apu.ch1.length.counter, 64);
    assert_eq!(rig.status() & 1, 1);
    rig.w(0xFF12, 0x00); // DAC off disables the channel immediately
    assert_eq!(rig.status() & 1, 0);
}

#[test]
fn envelope_decreases_every_pace_times_8_steps() {
    let mut rig = Rig::new();
    rig.w(0xFF12, 0x82); // volume 8, decrease, pace 2
    rig.w(0xFF14, 0x80);
    assert_eq!(rig.apu.ch1.env.volume, 8);
    rig.seq_ticks(8); // one envelope clock (step 7): timer 2 -> 1
    assert_eq!(rig.apu.ch1.env.volume, 8);
    rig.seq_ticks(8);
    assert_eq!(rig.apu.ch1.env.volume, 7);
    rig.seq_ticks(16 * 8);
    assert_eq!(rig.apu.ch1.env.volume, 0, "saturates at 0");
    assert_eq!(rig.status() & 1, 1, "envelope never turns the channel off");
}

#[test]
fn envelope_increases_and_saturates_at_15() {
    let mut rig = Rig::new();
    rig.w(0xFF17, 0x79); // volume 7, increase, pace 1
    rig.w(0xFF19, 0x80);
    assert_eq!(rig.apu.ch2.env.volume, 7);
    rig.seq_ticks(8);
    assert_eq!(rig.apu.ch2.env.volume, 8);
    rig.seq_ticks(8 * 20);
    assert_eq!(rig.apu.ch2.env.volume, 15);
}

#[test]
fn envelope_pace_zero_holds_volume() {
    let mut rig = Rig::new();
    rig.w(0xFF12, 0xA0);
    rig.w(0xFF14, 0x80);
    rig.seq_ticks(64);
    assert_eq!(rig.apu.ch1.env.volume, 10);
}

/// CH1 triggered with `nr12`, ready for NR12 rewrites.
fn ch1_with(nr12: u8) -> Rig {
    let mut rig = Rig::new();
    rig.w(0xFF12, nr12);
    rig.w(0xFF14, 0x80);
    rig
}

fn ch1_volume(rig: &Rig) -> u8 {
    rig.apu.ch1.env.volume
}

#[test]
fn zombie_period_zero_increase_adds_one_per_write() {
    // The documented "manual volume" idiom: $V8 then repeated $08.
    let mut rig = ch1_with(0x58);
    rig.w(0xFF12, 0x08);
    assert_eq!(ch1_volume(&rig), 6);
    for _ in 0..14 {
        rig.w(0xFF12, 0x08);
    }
    assert_eq!(ch1_volume(&rig), 4, "5 + 15 wraps to 4: decrement by one");
}

#[test]
fn zombie_decrease_mode_adds_two() {
    let mut rig = ch1_with(0x61);
    rig.w(0xFF12, 0x61);
    assert_eq!(ch1_volume(&rig), 8);
    rig.w(0xFF12, 0x62); // pace change alone: still +2 (old was decrease)
    assert_eq!(ch1_volume(&rig), 10);
}

#[test]
fn zombie_increase_mode_with_nonzero_period_leaves_volume() {
    let mut rig = ch1_with(0x99);
    rig.w(0xFF12, 0x9A);
    assert_eq!(ch1_volume(&rig), 9);
}

#[test]
fn zombie_direction_flip_inverts_volume() {
    // decrease -> increase: (9 + 2) then 16 - 11.
    let mut rig = ch1_with(0x91);
    rig.w(0xFF12, 0x99);
    assert_eq!(ch1_volume(&rig), 5);
    // increase (period 1) -> decrease: no increment, then 16 - 5.
    rig.w(0xFF12, 0x91);
    assert_eq!(ch1_volume(&rig), 11);
    // volume 1, decrease -> increase: (1 + 2) then 16 - 3.
    let mut rig = ch1_with(0x11);
    rig.w(0xFF12, 0x08);
    assert_eq!(ch1_volume(&rig), 13);
}

#[test]
fn zombie_period_zero_needs_automatic_updates_still_running() {
    // Increase mode runs into 15: the envelope stops updating itself.
    let mut rig = ch1_with(0xF9);
    rig.seq_ticks(8);
    assert_eq!(ch1_volume(&rig), 15);
    rig.w(0xFF12, 0xF8); // period 0 now; old period 1, increase: no change
    assert_eq!(ch1_volume(&rig), 15);
    // Old period is 0 but updates have stopped: +0 instead of +1 (-> 0).
    rig.w(0xFF12, 0x08);
    assert_eq!(ch1_volume(&rig), 15);
    // A retrigger restarts automatic updates.
    rig.w(0xFF12, 0xF8);
    rig.w(0xFF14, 0x80);
    rig.w(0xFF12, 0x08);
    assert_eq!(ch1_volume(&rig), 0, "15 + 1 wraps");
}

#[test]
fn zombie_does_not_apply_to_a_channel_that_is_off() {
    let mut rig = Rig::new();
    rig.w(0xFF12, 0x58);
    rig.w(0xFF12, 0x08);
    assert_eq!(ch1_volume(&rig), 0);
    // Trigger then length expiry: off channels keep their volume on writes.
    rig.w(0xFF12, 0x58);
    rig.w(0xFF14, 0x80);
    rig.w(0xFF12, 0x00); // DAC off disables the channel
    let v = ch1_volume(&rig);
    rig.w(0xFF12, 0x08);
    assert_eq!(ch1_volume(&rig), v);
}

#[test]
fn zombie_applies_to_ch2_and_ch4() {
    let mut rig = Rig::new();
    rig.w(0xFF17, 0x58);
    rig.w(0xFF19, 0x80);
    rig.w(0xFF17, 0x08);
    assert_eq!(rig.apu.ch2.env.volume, 6);
    rig.w(0xFF21, 0x91);
    rig.w(0xFF23, 0x80);
    rig.w(0xFF21, 0x99);
    assert_eq!(rig.apu.ch4.env.volume, 5);
}

#[test]
fn envelope_trigger_before_envelope_step_gets_one_extra_period() {
    // Triggered while the next step (7) clocks the envelope: that clock only
    // consumes the extra tick, so the first decrement is a full period late.
    let mut rig = Rig::new();
    rig.w(0xFF12, 0x81); // volume 8, decrease, period 1
    rig.seq_ticks(7);
    assert_eq!(rig.apu.seq_step, 7);
    rig.w(0xFF14, 0x80);
    rig.seq_ticks(1); // step 7
    assert_eq!(ch1_volume(&rig), 8);
    rig.seq_ticks(7);
    assert_eq!(ch1_volume(&rig), 8);
    rig.seq_ticks(1); // next step 7
    assert_eq!(ch1_volume(&rig), 7);
}

#[test]
fn envelope_trigger_before_other_steps_has_no_extra_period() {
    let mut rig = Rig::new();
    rig.w(0xFF12, 0x81);
    rig.seq_ticks(6);
    assert_eq!(rig.apu.seq_step, 6);
    rig.w(0xFF14, 0x80);
    rig.seq_ticks(1); // step 6
    assert_eq!(ch1_volume(&rig), 8);
    rig.seq_ticks(1); // step 7: first clock decrements
    assert_eq!(ch1_volume(&rig), 7);
}

#[test]
fn envelope_trigger_quirk_applies_to_ch2_and_ch4() {
    let mut rig = Rig::new();
    rig.w(0xFF17, 0x81);
    rig.w(0xFF21, 0x81);
    rig.seq_ticks(7);
    rig.w(0xFF19, 0x80);
    rig.w(0xFF23, 0x80);
    rig.seq_ticks(1);
    assert_eq!(rig.apu.ch2.env.volume, 8);
    assert_eq!(rig.apu.ch4.env.volume, 8);
    rig.seq_ticks(8);
    assert_eq!(rig.apu.ch2.env.volume, 7);
    assert_eq!(rig.apu.ch4.env.volume, 7);
}

#[test]
fn envelope_period_zero_timer_keeps_counting_until_period_is_set() {
    // Period 0 reloads as 8 but never steps; the timer still runs, so setting
    // a period later takes effect when the running timer expires.
    let mut rig = ch1_with(0x58);
    rig.seq_ticks(8 * 3); // timer 8 -> 5
    rig.w(0xFF12, 0x59); // old period 0, updating: zombie +1 -> 6; new period 1
    assert_eq!(ch1_volume(&rig), 6);
    rig.seq_ticks(8 * 4); // timer 5 -> 1
    assert_eq!(ch1_volume(&rig), 6);
    rig.seq_ticks(8);
    assert_eq!(ch1_volume(&rig), 7);
}

#[test]
fn sweep_up_updates_frequency_then_overflow_disables() {
    let mut rig = Rig::new();
    rig.w(0xFF12, 0xF0);
    rig.w(0xFF10, 0x11); // pace 1, add, shift 1
    rig.w(0xFF13, 0x00);
    rig.w(0xFF14, 0x84); // freq 0x400
    assert_eq!(rig.status() & 1, 1);
    rig.seq_ticks(2); // steps 0, 1: no sweep clock
    assert_eq!(rig.apu.ch1.freq, 0x400);
    rig.seq_ticks(1); // step 2: 0x400 + 0x200 = 0x600; next calc 0x900 overflows
    assert_eq!(rig.apu.ch1.freq, 0x600);
    assert_eq!(rig.status() & 1, 0);
}

#[test]
fn sweep_down_decreases_frequency() {
    let mut rig = Rig::new();
    rig.w(0xFF12, 0xF0);
    rig.w(0xFF10, 0x19); // pace 1, subtract, shift 1
    rig.w(0xFF13, 0x00);
    rig.w(0xFF14, 0x84);
    rig.seq_ticks(3);
    assert_eq!(rig.apu.ch1.freq, 0x200);
    rig.seq_ticks(4); // step 6
    assert_eq!(rig.apu.ch1.freq, 0x100);
    assert_eq!(rig.status() & 1, 1);
}

#[test]
fn sweep_overflow_check_at_trigger() {
    let mut rig = Rig::new();
    rig.w(0xFF12, 0xF0);
    rig.w(0xFF10, 0x11);
    rig.w(0xFF13, 0xFF);
    rig.w(0xFF14, 0x87); // freq 0x7FF + 0x3FF > 0x7FF
    assert_eq!(rig.status() & 1, 0);
}

#[test]
fn sweep_shift_zero_does_not_touch_frequency() {
    let mut rig = Rig::new();
    rig.w(0xFF12, 0xF0);
    rig.w(0xFF10, 0x10); // pace 1, shift 0
    rig.w(0xFF13, 0x34);
    rig.w(0xFF14, 0x82);
    rig.seq_ticks(8);
    assert_eq!(rig.apu.ch1.freq, 0x234);
    assert_eq!(rig.status() & 1, 1);
}

#[test]
fn sweep_negate_cleared_after_use_disables_channel() {
    let mut rig = Rig::new();
    rig.w(0xFF12, 0xF0);
    rig.w(0xFF10, 0x19); // subtract: trigger-time calculation sets negate_used
    rig.w(0xFF14, 0x84);
    assert_eq!(rig.status() & 1, 1);
    rig.w(0xFF10, 0x11); // negate bit cleared
    assert_eq!(rig.status() & 1, 0);
}

#[test]
fn square_duty_and_frequency_drive_output() {
    let mut rig = Rig::new();
    rig.w(0xFF17, 0xF0);
    rig.w(0xFF16, 0x80); // duty 50%
    rig.w(0xFF18, 0x00);
    rig.w(0xFF19, 0x86); // freq 0x600: 2048 - 1536 = 512 M-cycles per step
    let mut seen = Vec::new();
    for _ in 0..8 {
        seen.push(rig.apu.ch2.output());
        rig.run(512);
    }
    assert_eq!(seen, [15, 0, 0, 0, 0, 15, 15, 15]);
}

/// Wave RAM holding nibbles 0,1,2,...,15,0,1,...,15.
fn load_ramp(rig: &mut Rig) {
    for i in 0..16u8 {
        let hi = (2 * i) & 15;
        let lo = (2 * i + 1) & 15;
        rig.w(0xFF30 + u16::from(i), hi << 4 | lo);
    }
}

fn start_wave(rig: &mut Rig, level: u8, freq: u16) {
    rig.w(0xFF1A, 0x80);
    rig.w(0xFF1C, level << 5);
    rig.w(0xFF1D, freq as u8);
    rig.w(0xFF1E, 0x80 | (freq >> 8) as u8);
}

#[test]
fn wave_plays_ram_high_nibble_first_and_wraps() {
    let mut rig = Rig::new();
    load_ramp(&mut rig);
    start_wave(&mut rig, 1, 2046); // 4 T-cycles (1 M-cycle) per sample
    assert_eq!(rig.status() & 4, 4);
    // The first fetch comes 6 T-cycles after the period; skip to just before it.
    rig.run(2);
    // Trigger does not refetch: the first fetch is nibble 1 (low half of byte
    // 0), nibble 0 is only reached after wrapping (Pan Docs: CH3 trigger).
    let mut out = Vec::new();
    for _ in 0..40 {
        rig.run(1);
        out.push(rig.apu.ch3.output());
    }
    let expect: Vec<u8> = (1..=40u8).map(|n| n & 15).collect();
    assert_eq!(out, expect);
}

#[test]
fn wave_high_frequency_fetches_twice_per_mcycle() {
    let mut rig = Rig::new();
    load_ramp(&mut rig);
    start_wave(&mut rig, 1, 2047); // 2 T-cycles per sample
    // Fetches at T = 8, 10, ..., 16 (6 T trigger delay + 2 T period): nibble 5.
    rig.run(4);
    assert_eq!(rig.apu.ch3.output(), 5);
}

#[test]
fn wave_output_levels_shift_sample() {
    for (level, expect) in [(0u8, 0u8), (1, 12), (2, 6), (3, 3)] {
        let mut rig = Rig::new();
        rig.w(0xFF30, 0x0C);
        start_wave(&mut rig, level, 2046);
        rig.run(3); // past the first fetch (10 T after the trigger)
        assert_eq!(rig.apu.ch3.output(), expect, "level code {level}");
    }
}

#[test]
fn wave_length_is_256_and_dac_off_disables() {
    let mut rig = Rig::new();
    rig.w(0xFF1A, 0x80);
    rig.w(0xFF1B, 0xFE); // 256 - 254 = 2
    rig.w(0xFF1E, 0xC0);
    assert_eq!(rig.status() & 4, 4);
    rig.seq_ticks(3);
    assert_eq!(rig.status() & 4, 0);
    rig.w(0xFF1B, 0x00);
    rig.w(0xFF1E, 0x80);
    assert_eq!(rig.apu.ch3.length.counter, 256);
    assert_eq!(rig.status() & 4, 4);
    rig.w(0xFF1A, 0x00);
    assert_eq!(rig.status() & 4, 0);
}

#[test]
fn wave_ram_is_gated_while_channel_plays() {
    let mut rig = Rig::new();
    rig.w(0xFF35, 0x5A);
    assert_eq!(rig.apu.read(0xFF35), 0x5A);
    start_wave(&mut rig, 1, 0); // slow: a fetch every 4096 M-cycles
    rig.run(10);
    assert_eq!(rig.apu.read(0xFF35), 0xFF);
    rig.w(0xFF35, 0x11);
    rig.w(0xFF1A, 0x00);
    assert_eq!(rig.apu.read(0xFF35), 0x5A, "write while playing was lost");
}

/// Independent LFSR model in the other common convention (state complemented):
/// starts at $7FFF, shifts in the XOR of bits 0 and 1, and is high when bit 0
/// is clear. It must match the Pan Docs XNOR/init-0 formulation bit for bit.
fn reference_noise(short: bool, clocks: usize) -> Vec<bool> {
    let mut l: u16 = 0x7FFF;
    let mut out = Vec::new();
    for _ in 0..clocks {
        let x = (l ^ (l >> 1)) & 1;
        l >>= 1;
        l = l & !(1 << 14) | x << 14;
        if short {
            l = l & !(1 << 6) | x << 6;
        }
        out.push(l & 1 == 0);
    }
    out
}

/// Trigger CH4 at volume 15 with shift 0 / divisor code 1 (16 T = 4 M-cycles
/// per LFSR clock) and sample the output after each clock.
fn noise_outputs(short: bool, clocks: usize) -> Vec<bool> {
    let mut rig = Rig::new();
    rig.w(0xFF21, 0xF0);
    rig.w(0xFF22, 0x01 | if short { 0x08 } else { 0 });
    rig.w(0xFF23, 0x80);
    (0..clocks)
        .map(|_| {
            rig.run(4);
            rig.apu.ch4.output() == 15
        })
        .collect()
}

#[test]
fn lfsr_7bit_hand_computed_start() {
    // From state 0 each clock shifts in XNOR(b0, b1) at bit 6: six 1s are
    // shifted in before bit 0 is reached, then the pattern continues.
    let got = noise_outputs(true, 9);
    assert_eq!(
        got,
        [false, false, false, false, false, false, true, true, true]
    );
}

#[test]
fn lfsr_matches_reference_in_both_widths() {
    assert_eq!(noise_outputs(false, 400), reference_noise(false, 400));
    assert_eq!(noise_outputs(true, 400), reference_noise(true, 400));
}

#[test]
fn lfsr_period_is_127_short_and_32767_long() {
    let mut rig = Rig::new();
    rig.w(0xFF21, 0xF0);
    // In 7-bit mode only the low 7 bits form the register; the upper bits are
    // shifted-out history.
    for (nr43, period, mask) in [(0x09u8, 127u32, 0x7Fu16), (0x01, 32767, 0x7FFF)] {
        rig.w(0xFF22, nr43);
        rig.w(0xFF23, 0x80);
        let start = rig.apu.ch4.lfsr & mask;
        for n in 1..=period {
            rig.apu.ch4.clock_lfsr();
            let back = rig.apu.ch4.lfsr & mask == start;
            assert_eq!(back, n == period, "nr43 {nr43:#x} n {n}");
        }
    }
}

#[test]
fn noise_clock_shift_14_and_15_never_clock() {
    let mut rig = Rig::new();
    rig.w(0xFF21, 0xF0);
    rig.w(0xFF22, 0xE0);
    rig.w(0xFF23, 0x80);
    rig.run(100_000);
    assert_eq!(rig.apu.ch4.lfsr, 0);
    assert_eq!(rig.apu.ch4.output(), 0);
}

#[test]
fn noise_period_follows_divisor_and_shift() {
    // Divisor code 0 = 8 T, shift 2 -> 32 T = 8 M-cycles per clock.
    let mut rig = Rig::new();
    rig.w(0xFF21, 0xF0);
    rig.w(0xFF22, 0x20);
    rig.w(0xFF23, 0x80);
    rig.run(7);
    assert_eq!(rig.apu.ch4.lfsr, 0);
    rig.run(1);
    assert_eq!(rig.apu.ch4.lfsr, 0x4000);
}

#[test]
fn length_expires_for_noise() {
    let mut rig = Rig::new();
    rig.w(0xFF21, 0xF0);
    rig.w(0xFF20, 0x3E);
    rig.w(0xFF23, 0xC0);
    assert_eq!(rig.status() & 8, 8);
    rig.seq_ticks(3);
    assert_eq!(rig.status() & 8, 0);
}

fn drain(apu: &mut Apu) -> Vec<f32> {
    let mut out = Vec::new();
    apu.drain_samples(&mut out);
    out
}

/// Start CH2 as an audible 50% square at volume 15.
fn start_ch2(rig: &mut Rig) {
    rig.w(0xFF17, 0xF0);
    rig.w(0xFF16, 0x80);
    rig.w(0xFF18, 0x00);
    rig.w(0xFF19, 0x80 | 0x07); // freq 0x700: fast enough to toggle many times
}

#[test]
fn post_boot_registers_read_back_like_the_phase1_stub() {
    let apu = Apu::new();
    let expect: [u8; 32] = [
        0x80, 0xBF, 0xF3, 0xFF, 0xBF, 0xFF, 0x3F, 0x00, 0xFF, 0xBF, 0x7F, 0xFF, 0x9F, 0xFF, 0xBF,
        0xFF, 0xFF, 0x00, 0x00, 0xBF, 0x77, 0xF3, 0xF1, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFF, 0xFF,
    ];
    for (i, &e) in expect.iter().enumerate() {
        let addr = 0xFF10 + i as u16;
        assert_eq!(apu.read(addr), e, "{addr:#06x}");
    }
    for addr in 0xFF30..=0xFF3F {
        assert_eq!(apu.read(addr), 0, "wave RAM {addr:#06x}");
    }
    assert_eq!(apu.read(0xFF26), 0xF1, "boot_hwio: CH1 active");
}

#[test]
fn post_boot_period_registers_start_at_ff_for_all_three_channels() {
    // Pan Docs "Power Up Sequence": NR13, NR23 and NR33 are $FF (write-only,
    // so only the internal period shows) and NR14/NR24/NR34 carry 0x3F.
    let apu = Apu::new();
    assert_eq!(apu.ch1.freq, 0x7FF);
    assert_eq!(apu.ch2.freq, 0x7FF);
    assert_eq!(apu.ch3.freq, 0x7FF);
}

#[test]
fn register_read_masks_hide_write_only_bits() {
    let mut apu = fresh();
    let write_only_bits = |i: usize| READ_MASK[i];
    for addr in 0xFF10..=0xFF25u16 {
        if addr == 0xFF15 {
            continue;
        }
        apu.write(addr, 0xFF);
        // CH3 NR30 write of $FF also leaves the DAC on; reads are unaffected.
        assert_eq!(apu.read(addr), 0xFF, "{addr:#06x} after $FF");
        apu.write(addr, 0x00);
        let i = (addr - 0xFF10) as usize;
        assert_eq!(apu.read(addr), write_only_bits(i), "{addr:#06x} after $00");
    }
    assert_eq!(apu.read(0xFF15), 0xFF);
    assert_eq!(apu.read(0xFF27), 0xFF, "unused registers read $FF");
}

#[test]
fn nr52_reports_channel_status_and_only_bit7_is_writable() {
    let mut rig = Rig::new();
    assert_eq!(rig.apu.read(0xFF26), 0xF0);
    rig.w(0xFF17, 0xF0);
    rig.w(0xFF19, 0x80);
    assert_eq!(rig.apu.read(0xFF26), 0xF2);
    rig.w(0xFF26, 0x8F); // low bits are read-only
    assert_eq!(rig.apu.read(0xFF26), 0xF2);
}

#[test]
fn nr52_power_off_clears_registers_and_ignores_writes() {
    let mut rig = Rig::new();
    rig.w(0xFF24, 0x77);
    rig.w(0xFF25, 0xFF);
    rig.w(0xFF12, 0xF3);
    rig.w(0xFF14, 0x80);
    rig.w(0xFF30, 0xAB);
    rig.w(0xFF26, 0x00);
    assert_eq!(rig.apu.read(0xFF26), 0x70);
    for addr in 0xFF10..=0xFF25u16 {
        let i = (addr - 0xFF10) as usize;
        assert_eq!(rig.apu.read(addr), READ_MASK[i], "{addr:#06x}");
    }
    assert_eq!(rig.apu.read(0xFF30), 0xAB, "wave RAM survives power-off");

    rig.w(0xFF12, 0xF0);
    rig.w(0xFF24, 0x55);
    rig.w(0xFF26, 0x08); // bit 7 clear: stays off
    assert_eq!(rig.apu.read(0xFF12), 0x00);
    assert_eq!(rig.apu.read(0xFF24), 0x00);
    rig.w(0xFF31, 0xCD);
    assert_eq!(rig.apu.read(0xFF31), 0xCD, "wave RAM writable while off");
}

#[test]
fn nr52_power_off_keeps_length_counters_writable_on_dmg() {
    let mut rig = Rig::new();
    rig.w(0xFF26, 0x00);
    rig.w(0xFF11, 0xFE); // duty bits (3) must be dropped; length 64-62 = 2
    rig.w(0xFF1B, 0xFE);
    rig.w(0xFF20, 0x3E);
    rig.w(0xFF26, 0x80);
    assert_eq!(rig.apu.read(0xFF11), 0x3F, "duty not written while off");
    assert_eq!(rig.apu.ch1.length.counter, 2);
    assert_eq!(rig.apu.ch3.length.counter, 2);
    assert_eq!(rig.apu.ch4.length.counter, 2);
}

#[test]
fn power_off_silences_channels_and_power_on_restarts_sequencer() {
    let mut rig = Rig::new();
    start_ch2(&mut rig);
    rig.seq_ticks(3);
    assert_eq!(rig.apu.seq_step, 3);
    rig.w(0xFF26, 0x00);
    assert_eq!(rig.apu.read(0xFF26) & 0x0F, 0);
    rig.seq_ticks(2);
    rig.w(0xFF26, 0x80);
    assert_eq!(rig.apu.seq_step, 0);
    assert_eq!(rig.apu.read(0xFF26), 0xF0);
}

#[test]
fn silent_apu_produces_exact_zero_samples() {
    for mut rig in [
        Rig::new(),
        Rig {
            apu: Apu::new(),
            div: 0,
        },
    ] {
        rig.run(M_CYCLES_PER_FRAME_TEST * 3);
        let s = drain(&mut rig.apu);
        assert!(!s.is_empty());
        assert!(s.iter().all(|&x| x == 0.0));
    }
}

#[test]
fn powered_off_apu_stays_silent_even_with_stale_pan_state() {
    let mut rig = Rig::new();
    start_ch2(&mut rig);
    rig.w(0xFF24, 0x77);
    rig.w(0xFF25, 0xFF);
    rig.w(0xFF26, 0x00);
    rig.run(5000);
    assert!(drain(&mut rig.apu).iter().all(|&x| x == 0.0));
}

const M_CYCLES_PER_FRAME_TEST: u32 = crate::M_CYCLES_PER_FRAME;

#[test]
fn triggered_channel_produces_signed_bounded_samples() {
    let mut rig = Rig::new();
    rig.w(0xFF24, 0x77);
    rig.w(0xFF25, 0xFF);
    start_ch2(&mut rig);
    rig.run(M_CYCLES_PER_FRAME_TEST);
    let s = drain(&mut rig.apu);
    assert!(s.iter().all(|x| x.is_finite() && (-1.0..=1.0).contains(x)));
    assert!(s.iter().any(|&x| x > 0.1), "positive half");
    assert!(s.iter().any(|&x| x < -0.1), "negative half");
    // One channel out of four at master volume 8/8 swings +-0.25; the filter
    // can at most double a step.
    let peak = s.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!(peak <= 0.5 + 1e-6, "{peak}");
    // Interleaved L/R are identical with symmetric panning.
    assert!(s.chunks(2).all(|p| p[0] == p[1]));
}

#[test]
fn dac_is_bipolar_and_a_powered_down_dac_is_zero() {
    let mut rig = Rig::new();
    rig.w(0xFF24, 0x77);
    rig.w(0xFF25, 0x22); // CH2 on both sides
    rig.w(0xFF16, 0x80); // duty 50%: first step is high
    rig.w(0xFF17, 0xF0);
    rig.w(0xFF19, 0x80);
    rig.apu.update_mix();
    assert_eq!(rig.apu.cur_l, -15 * 8, "digital 15 -> analog -1, master 8");
    rig.w(0xFF17, 0x08); // volume 0 with the DAC on: digital 0 -> analog +1
    rig.w(0xFF19, 0x80);
    rig.apu.update_mix();
    assert_eq!(rig.apu.cur_l, 15 * 8);
    rig.w(0xFF17, 0x00); // DAC off: contributes nothing at all
    rig.apu.update_mix();
    assert_eq!((rig.apu.cur_l, rig.apu.cur_r), (0, 0));
    assert!(!rig.apu.dac_any);
}

#[test]
fn hpf_decays_dc_from_an_enabled_but_silent_dac() {
    let mut rig = Rig::new();
    rig.w(0xFF24, 0x77);
    rig.w(0xFF25, 0xFF);
    rig.w(0xFF17, 0x08); // DAC on, volume 0: constant analog +1 on CH2
    rig.w(0xFF19, 0x80);
    rig.run(M_CYCLES_PER_FRAME_TEST * 60); // ~1 s
    let s = drain(&mut rig.apu);
    assert!((s[0] - 0.25).abs() < 1e-6, "step passes through: {}", s[0]);
    assert!(s[2] < s[0] && s[2] > 0.0, "decays monotonically");
    let n = s.len();
    assert!(s[n - 2000..].iter().all(|x| x.abs() < 1e-3), "{}", s[n - 1]);
}

#[test]
fn hpf_centres_an_off_centre_waveform() {
    let mut rig = Rig::new();
    rig.w(0xFF24, 0x77);
    rig.w(0xFF25, 0xFF);
    rig.w(0xFF16, 0x00); // 12.5% duty: raw mean is far from 0
    rig.w(0xFF17, 0xF0);
    rig.w(0xFF18, 0x00);
    rig.w(0xFF19, 0x87);
    rig.run(M_CYCLES_PER_FRAME_TEST * 60);
    let s = drain(&mut rig.apu);
    let tail = &s[s.len() / 2..];
    let mean = tail.iter().sum::<f32>() / tail.len() as f32;
    assert!(mean.abs() < 0.01, "{mean}");
    assert!(tail.iter().any(|x| x.abs() > 0.1), "signal survives");
}

#[test]
fn channel_start_and_stop_do_not_click() {
    // A running tone is silenced by zeroing the volume (DAC stays on): the DC
    // step this leaves behind fades instead of persisting.
    let mut rig = Rig::new();
    rig.w(0xFF24, 0x77);
    rig.w(0xFF25, 0xFF);
    start_ch2(&mut rig);
    rig.run(M_CYCLES_PER_FRAME_TEST * 30);
    drain(&mut rig.apu);
    rig.w(0xFF17, 0x08);
    rig.w(0xFF19, 0x80);
    rig.run(M_CYCLES_PER_FRAME_TEST * 60);
    let s = drain(&mut rig.apu);
    assert!(s[s.len() - 2000..].iter().all(|x| x.abs() < 1e-3));
}

#[test]
fn all_dacs_off_is_exactly_zero_and_resumes_when_one_returns() {
    let mut rig = Rig::new();
    rig.w(0xFF24, 0x77);
    rig.w(0xFF25, 0xFF);
    start_ch2(&mut rig);
    rig.run(M_CYCLES_PER_FRAME_TEST);
    drain(&mut rig.apu);
    rig.w(0xFF17, 0x00);
    rig.run(M_CYCLES_PER_FRAME_TEST);
    let s = drain(&mut rig.apu);
    assert!(!s.is_empty() && s.iter().all(|&x| x == 0.0));
    start_ch2(&mut rig);
    rig.run(M_CYCLES_PER_FRAME_TEST);
    assert!(drain(&mut rig.apu).iter().any(|&x| x != 0.0));
}

#[test]
fn power_off_after_activity_is_exactly_zero() {
    let mut rig = Rig::new();
    rig.w(0xFF24, 0x77);
    rig.w(0xFF25, 0xFF);
    start_ch2(&mut rig);
    rig.run(M_CYCLES_PER_FRAME_TEST);
    drain(&mut rig.apu);
    rig.w(0xFF26, 0x00);
    rig.run(M_CYCLES_PER_FRAME_TEST);
    let s = drain(&mut rig.apu);
    assert!(!s.is_empty() && s.iter().all(|&x| x == 0.0));
}

#[test]
fn full_scale_mix_stays_finite_and_within_range() {
    let mut rig = Rig::new();
    rig.w(0xFF24, 0x77);
    rig.w(0xFF25, 0xFF);
    // All four channels at full volume, fast, with wave RAM alternating 0/F.
    for i in 0..16u16 {
        rig.w(0xFF30 + i, 0x0F);
    }
    rig.w(0xFF17, 0xF0);
    rig.w(0xFF19, 0x87);
    rig.w(0xFF12, 0xF0);
    rig.w(0xFF14, 0x87);
    rig.w(0xFF1A, 0x80);
    rig.w(0xFF1C, 0x20);
    rig.w(0xFF1E, 0x87);
    rig.w(0xFF21, 0xF0);
    rig.w(0xFF22, 0x00);
    rig.w(0xFF23, 0x80);
    for round in 0..40 {
        rig.run(M_CYCLES_PER_FRAME_TEST / 4);
        // Toggle the pan and master volume to force big DC steps.
        rig.w(0xFF25, if round % 2 == 0 { 0x0F } else { 0xF0 });
        rig.w(0xFF24, if round % 3 == 0 { 0x00 } else { 0x77 });
    }
    let s = drain(&mut rig.apu);
    assert!(s.iter().all(|x| x.is_finite() && (-1.0..=1.0).contains(x)));
    assert!(s.iter().any(|&x| x != 0.0));
}

#[test]
fn hpf_factor_matches_the_documented_formula() {
    for rate in [8_000u32, 22_050, 44_100, 48_000, 96_000, 192_000] {
        let expect = 0.999_958f64.powf(4_194_304.0 / f64::from(rate));
        assert!(
            (f64::from(hpf_factor(rate)) - expect).abs() < 1e-7,
            "rate {rate}"
        );
    }
    let mut apu = Apu::new();
    apu.set_sample_rate(44_100);
    assert_eq!(apu.hpf_factor, hpf_factor(44_100));
}

#[test]
fn panning_and_master_volume_apply_per_side() {
    let mut rig = Rig::new();
    rig.w(0xFF24, 0x07); // left volume 1/8, right volume 1/8
    rig.w(0xFF25, 0x20); // CH2 left only
    start_ch2(&mut rig);
    rig.run(M_CYCLES_PER_FRAME_TEST);
    let s = drain(&mut rig.apu);
    assert!(s.chunks(2).all(|p| p[1] == 0.0));
    let left_max = s.chunks(2).map(|p| p[0]).fold(0.0, f32::max);
    assert!(left_max > 0.0);

    let mut rig = Rig::new();
    rig.w(0xFF24, 0x70); // left volume 8/8
    rig.w(0xFF25, 0x20);
    start_ch2(&mut rig);
    rig.run(M_CYCLES_PER_FRAME_TEST);
    let s = drain(&mut rig.apu);
    let loud_max = s.chunks(2).map(|p| p[0]).fold(0.0, f32::max);
    assert!((loud_max / left_max - 8.0).abs() < 1e-3);
}

#[test]
fn sample_count_matches_configured_rate() {
    for rate in [8_000u32, 22_050, 44_100, 48_000] {
        let mut rig = Rig::new();
        rig.apu.set_sample_rate(rate);
        assert_eq!(rig.apu.sample_rate(), rate);
        let frames = 60u32;
        rig.run(frames * M_CYCLES_PER_FRAME_TEST / 2);
        let n = drain(&mut rig.apu).len() / 2;
        let expect = (u64::from(frames * M_CYCLES_PER_FRAME_TEST / 2) * u64::from(rate) / 1_048_576)
            as usize;
        assert_eq!(n, expect, "rate {rate}");
        // Exactly 30 frames (~0.5 s) of audio, within one sample.
        let secs = f64::from(frames / 2) / crate::FRAMES_PER_SECOND;
        assert!(
            (n as f64 - secs * f64::from(rate)).abs() < 2.0,
            "rate {rate}"
        );
    }
}

#[test]
fn undrained_buffer_is_capped() {
    let mut rig = Rig::new();
    rig.run(2_200_000); // > 2 s at 48 kHz
    assert!(rig.apu.samples.len() <= rig.apu.max_buffered());
    assert_eq!(rig.apu.samples.len() % 2, 0);
    // The cap is one second at whatever rate is configured.
    let mut rig = Rig::new();
    rig.apu.set_sample_rate(8_000);
    rig.run(2_200_000);
    assert_eq!(rig.apu.samples.len(), 2 * 8_000);
}

#[test]
fn output_is_deterministic() {
    let run = || {
        let mut rig = Rig::new();
        rig.w(0xFF24, 0x77);
        rig.w(0xFF25, 0xFF);
        start_ch2(&mut rig);
        rig.w(0xFF21, 0xF1);
        rig.w(0xFF22, 0x33);
        rig.w(0xFF23, 0x80);
        rig.run(200_000);
        drain(&mut rig.apu)
    };
    assert_eq!(run(), run());
}

/// Wave RAM with byte i = i * 0x11, channel playing.
fn playing_ramp(freq: u16) -> Rig {
    let mut rig = Rig::new();
    for i in 0..16u8 {
        rig.w(0xFF30 + u16::from(i), i * 0x11);
    }
    start_wave(&mut rig, 1, freq);
    rig
}

#[test]
fn wave_ram_access_window_is_the_fetching_half_mcycle() {
    // Period 6 T: first fetch at T = 12 (end of the third M-cycle), then
    // T = 18, 24, ... Only an access landing in the second half of a fetch
    // M-cycle sees data, and it sees the byte being fetched.
    let mut rig = playing_ramp(2045);
    rig.run(2);
    assert_eq!(rig.apu.read(0xFF30), 0xFF, "no fetch yet: blocked");
    rig.run(1);
    assert_eq!(rig.apu.read(0xFF3F), 0x00, "fetched nibble 1, byte 0");
    rig.run(1);
    assert_eq!(
        rig.apu.read(0xFF30),
        0xFF,
        "fetch at T=18 is in a first half"
    );
}

#[test]
fn wave_retrigger_before_fetch_corrupts_wave_ram() {
    let mut rig = playing_ramp(2045);
    // Fetch at T = 12, 18, 24, ... : after 7 M-cycles (T = 28) the next fetch
    // (T = 30) is within 2 T-cycles; it reads nibble 4, i.e. byte 2, which
    // overwrites byte 0 (indices below 4 only touch byte 0).
    rig.run(7);
    rig.w(0xFF1E, 0x80);
    assert_eq!(rig.apu.ch3.ram[..4], [0x22, 0x11, 0x22, 0x33]);
}

/// Digital output of every channel with the DAC gating applied, summed per
/// NR51 side and scaled by NR50, written out independently of `update_mix`
/// (explicit per-bit tables instead of the shift loop).
fn expected_mix(apu: &Apu) -> (i32, i32) {
    if !apu.powered {
        return (0, 0);
    }
    let outs = [
        (apu.ch1.dac_on(), apu.ch1.output()),
        (apu.ch2.dac_on(), apu.ch2.output()),
        (apu.ch3.dac_on(), apu.ch3.output()),
        (apu.ch4.dac_on(), apu.ch4.output()),
    ];
    // NR51: bits 7..4 = CH4..CH1 left, bits 3..0 = CH4..CH1 right.
    let left_bit = [0x10u8, 0x20, 0x40, 0x80];
    let right_bit = [0x01u8, 0x02, 0x04, 0x08];
    let (mut l, mut r) = (0, 0);
    for (c, &(dac, o)) in outs.iter().enumerate() {
        let analog = if dac { 15 - 2 * i32::from(o) } else { 0 };
        if apu.regs[0x15] & left_bit[c] != 0 {
            l += analog;
        }
        if apu.regs[0x15] & right_bit[c] != 0 {
            r += analog;
        }
    }
    (
        l * (i32::from(apu.regs[0x14] >> 4 & 7) + 1),
        r * (i32::from(apu.regs[0x14] & 7) + 1),
    )
}

#[test]
fn mix_is_refreshed_at_a_sequencer_step_that_changes_a_channel() {
    // Length expiry at step 0 silences CH2 with no register write and no
    // channel-reported change: only the sequencer's `dirty` refreshes the mix.
    let mut rig = Rig::new();
    rig.w(0xFF24, 0x77);
    rig.w(0xFF25, 0xFF);
    rig.w(0xFF17, 0xF0);
    rig.w(0xFF16, 0x80 | 0x3F); // 50% duty (step 0 high), length counter 1
    rig.w(0xFF18, 0x00);
    rig.w(0xFF19, 0xC0); // trigger, length on, freq 0: no duty step for 2048 M-cycles
    rig.run(1);
    let playing = rig.apu.cur_l;
    assert_eq!(rig.status() & 2, 2);
    assert_ne!(playing, 15 * 8, "CH2 output must be non-zero here");
    rig.run(SEQ_PERIOD - 2); // right before the step-0 DIV-APU event
    assert_eq!(rig.apu.cur_l, playing);
    rig.run(1); // sequencer step 0 clocks length: CH2 off
    assert_eq!(rig.status() & 2, 0);
    assert_eq!(rig.apu.cur_l, 15 * 8, "stale mix after the sequencer step");
    assert_eq!((rig.apu.cur_l, rig.apu.cur_r), expected_mix(&rig.apu));
}

#[test]
fn ch4_volume_decays_through_the_envelope_clock() {
    let mut rig = Rig::new();
    rig.w(0xFF21, 0x81); // volume 8, decrease, pace 1
    rig.w(0xFF22, 0x00);
    rig.w(0xFF23, 0x80);
    rig.seq_ticks(7);
    assert_eq!(rig.apu.ch4.env.volume, 8);
    rig.seq_ticks(1); // step 7: first envelope clock
    assert_eq!(rig.apu.ch4.env.volume, 7);
}

#[test]
fn length_trigger_of_an_expired_counter_loses_one_before_a_length_step() {
    // Next step odd (does not clock length): enabled counter reloads to max - 1.
    let mut rig = Rig::new();
    rig.w(0xFF17, 0xF0);
    rig.w(0xFF16, 0x3F); // counter 1
    rig.w(0xFF19, 0xC0);
    rig.seq_ticks(1); // step 0 expires it; next step is 1 (odd)
    assert_eq!(rig.apu.ch2.length.counter, 0);
    rig.w(0xFF19, 0xC0);
    assert_eq!(rig.apu.ch2.length.counter, 63);

    // Next step even: full reload.
    let mut rig = Rig::new();
    rig.w(0xFF17, 0xF0);
    rig.w(0xFF16, 0x3F);
    rig.w(0xFF19, 0xC0);
    rig.seq_ticks(2); // step 1 clocks nothing; next step is 2 (even)
    assert_eq!(rig.apu.ch2.length.counter, 0);
    rig.w(0xFF19, 0xC0);
    assert_eq!(rig.apu.ch2.length.counter, 64);
}
