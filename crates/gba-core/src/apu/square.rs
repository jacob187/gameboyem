//! Tone channels 1 (with sweep) and 2. Register bytes follow the GB layout the
//! halfword registers are made of (GBATEK "GBA Sound Channel 1 - Tone & Sweep",
//! "Channel 2 - Tone").
//!
//! Adapted from `gb-core`'s `apu/square.rs` (DECISIONS #60). Time is in CPU
//! cycles, not M-cycles. Dropped: save states, the NR10 negate-clear quirk and
//! the DMG zombie-mode envelope write.

use super::common::{Envelope, Length, period_or_8};

/// Duty waveforms, one bit per step, first step first (GBATEK: duty 12.5%,
/// 25%, 50%, 75%).
const DUTY: [[u8; 8]; 4] = [
    [0, 0, 0, 0, 0, 0, 0, 1],
    [1, 0, 0, 0, 0, 0, 0, 1],
    [1, 0, 0, 0, 0, 1, 1, 1],
    [0, 1, 1, 1, 1, 1, 1, 0],
];

/// CPU cycles per duty step: the tone frequency is `131072 / (2048 - n)` Hz
/// for 8 duty steps, so a step lasts `16.78 MHz / (8 * 131072 / (2048 - n))`
/// = `16 * (2048 - n)` cycles (GBATEK SOUND1CNT_X).
const STEP_CYCLES: u32 = 16;

#[derive(Clone, Debug, Default)]
struct Sweep {
    /// Raw NR10: sweep time (6-4, units of 7.8 ms), direction (3, 1 = decrease),
    /// shift (2-0).
    nr10: u8,
    shadow: u16,
    timer: u8,
    enabled: bool,
}

impl Sweep {
    fn pace(&self) -> u8 {
        (self.nr10 >> 4) & 7
    }

    fn shift(&self) -> u8 {
        self.nr10 & 7
    }

    /// `X(t) = X(t-1) +/- X(t-1) / 2^n` (GBATEK SOUND1CNT_L). May exceed 2047,
    /// which the caller treats as overflow (the channel stops).
    fn calculate(&self) -> u16 {
        let delta = self.shadow >> self.shift();
        if self.nr10 & 8 != 0 {
            self.shadow - delta
        } else {
            self.shadow + delta
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Square {
    has_sweep: bool,
    pub enabled: bool,
    duty: u8,
    pos: u8,
    pub freq: u16,
    /// CPU cycles until the next duty step.
    timer: u32,
    pub length: Length,
    pub env: Envelope,
    sweep: Sweep,
}

impl Square {
    pub fn new(has_sweep: bool) -> Self {
        Self {
            has_sweep,
            enabled: false,
            duty: 0,
            pos: 0,
            freq: 0,
            timer: STEP_CYCLES,
            length: Length::new(64),
            env: Envelope::default(),
            sweep: Sweep::default(),
        }
    }

    /// SOUNDCNT_X bit 7 cleared: every register of the channel reads zero.
    pub fn reset(&mut self) {
        *self = Self::new(self.has_sweep);
    }

    pub fn write_nr10(&mut self, v: u8) {
        self.sweep.nr10 = v;
    }

    pub fn write_duty_length(&mut self, v: u8) {
        self.duty = v >> 6;
        self.length.load(v & 0x3F);
    }

    pub fn write_envelope(&mut self, v: u8) {
        self.env.nr2 = v;
        if !self.env.dac_on() {
            self.enabled = false;
        }
    }

    pub fn write_freq_lo(&mut self, v: u8) {
        self.freq = (self.freq & 0x700) | u16::from(v);
    }

    /// High byte of SOUNDxCNT_X: frequency bits 8-10, length flag (bit 6),
    /// restart (bit 7).
    pub fn write_freq_hi_ctrl(&mut self, v: u8, next_step_odd: bool) {
        self.freq = (self.freq & 0xFF) | (u16::from(v & 7) << 8);
        if self.length.set_enabled(v & 0x40 != 0, next_step_odd) {
            self.enabled = false;
        }
        if v & 0x80 != 0 {
            self.trigger(next_step_odd);
        }
    }

    fn trigger(&mut self, next_step_odd: bool) {
        self.length.trigger(next_step_odd);
        self.enabled = self.env.dac_on();
        self.timer = self.period();
        self.env.trigger();
        if self.has_sweep {
            let s = &mut self.sweep;
            s.shadow = self.freq;
            s.timer = period_or_8(s.pace());
            s.enabled = s.pace() != 0 || s.shift() != 0;
            if s.shift() != 0 && s.calculate() > 2047 {
                self.enabled = false;
            }
        }
    }

    fn period(&self) -> u32 {
        STEP_CYCLES * (2048 - u32::from(self.freq))
    }

    pub fn clock_length(&mut self) {
        if self.length.clock() {
            self.enabled = false;
        }
    }

    pub fn clock_envelope(&mut self) {
        self.env.clock();
    }

    /// 128 Hz sweep clock (frame-sequencer steps 2 and 6).
    pub fn clock_sweep(&mut self) {
        if !self.has_sweep {
            return;
        }
        let s = &mut self.sweep;
        s.timer = s.timer.saturating_sub(1);
        if s.timer != 0 {
            return;
        }
        s.timer = period_or_8(s.pace());
        if !s.enabled || s.pace() == 0 {
            return;
        }
        let new = s.calculate();
        if new > 2047 {
            self.enabled = false;
        } else if s.shift() != 0 {
            s.shadow = new;
            self.freq = new;
            // The GB family checks the next step for overflow at once.
            if s.calculate() > 2047 {
                self.enabled = false;
            }
        }
    }

    /// Cycles until the next duty step, if the channel is running.
    pub fn next_step(&self) -> Option<u32> {
        self.enabled.then_some(self.timer)
    }

    /// Advance by `cycles` (never more than [`Square::next_step`]); returns
    /// `true` if the output level changed.
    pub fn advance(&mut self, cycles: u32) -> bool {
        if !self.enabled {
            return false;
        }
        self.timer -= cycles;
        if self.timer != 0 {
            return false;
        }
        self.timer = self.period();
        let before = self.output();
        self.pos = (self.pos + 1) & 7;
        self.env.volume != 0 && self.output() != before
    }

    /// Digital output 0..=15.
    pub fn output(&self) -> u8 {
        if self.enabled {
            DUTY[usize::from(self.duty)][usize::from(self.pos)] * self.env.volume
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect the duty bits over one full cycle at volume 1.
    fn duty_pattern(duty: u8) -> Vec<u8> {
        let mut s = Square::new(false);
        s.write_envelope(0x10);
        s.write_duty_length(duty << 6);
        s.write_freq_lo(0xFF);
        s.write_freq_hi_ctrl(0x87, false); // freq 0x7FF: 16 cycles per step
        let mut out = vec![s.output()];
        for _ in 0..7 {
            s.advance(s.next_step().unwrap());
            out.push(s.output());
        }
        out
    }

    #[test]
    fn duty_patterns() {
        assert_eq!(duty_pattern(0), [0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(duty_pattern(1), [1, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(duty_pattern(2), [1, 0, 0, 0, 0, 1, 1, 1]);
        assert_eq!(duty_pattern(3), [0, 1, 1, 1, 1, 1, 1, 0]);
    }

    #[test]
    fn step_lasts_16_times_2048_minus_n_cycles() {
        let mut s = Square::new(false);
        s.write_envelope(0xF0);
        s.write_freq_lo(0x00);
        s.write_freq_hi_ctrl(0x84, false); // n = 0x400 = 1024
        assert_eq!(s.next_step(), Some(16 * 1024));
    }

    #[test]
    fn dac_off_blocks_the_trigger() {
        let mut s = Square::new(false);
        s.write_envelope(0x00);
        s.write_freq_hi_ctrl(0x80, false);
        assert!(!s.enabled);
        s.write_envelope(0x08);
        s.write_freq_hi_ctrl(0x80, false);
        assert!(s.enabled, "increasing from 0 still powers the DAC");
        s.write_envelope(0x00);
        assert!(!s.enabled, "powering the DAC off stops the channel");
    }

    #[test]
    fn length_expiry_disables() {
        let mut s = Square::new(false);
        s.write_envelope(0xF0);
        s.write_duty_length(62); // two length clocks left
        s.write_freq_hi_ctrl(0xC0, false);
        s.clock_length();
        assert!(s.enabled);
        s.clock_length();
        assert!(!s.enabled);
    }

    #[test]
    fn length_ignored_without_the_flag() {
        let mut s = Square::new(false);
        s.write_envelope(0xF0);
        s.write_duty_length(63);
        s.write_freq_hi_ctrl(0x80, false);
        for _ in 0..10 {
            s.clock_length();
        }
        assert!(s.enabled);
    }

    fn sweep_channel(nr10: u8, freq: u16) -> Square {
        let mut s = Square::new(true);
        s.write_nr10(nr10);
        s.write_envelope(0xF0);
        s.write_freq_lo(freq as u8);
        s.write_freq_hi_ctrl(0x80 | (freq >> 8) as u8, false);
        s
    }

    #[test]
    fn sweep_up_raises_the_frequency_each_step_time() {
        // Time 1, increase, shift 1: X += X / 2 every 7.8 ms.
        let mut s = sweep_channel(0x11, 0x100);
        s.clock_sweep();
        assert_eq!(s.freq, 0x180);
        s.clock_sweep();
        assert_eq!(s.freq, 0x240);
        assert!(s.enabled);
    }

    #[test]
    fn sweep_down_lowers_it() {
        let mut s = sweep_channel(0x19, 0x200);
        s.clock_sweep();
        assert_eq!(s.freq, 0x100);
    }

    #[test]
    fn sweep_overflow_disables_channel_1() {
        // 0x300 -> 0x480 -> 0x6C0; the calculation after the second update
        // would be 0xA20 > 2047, so the channel stops there.
        let mut s = sweep_channel(0x11, 0x300);
        s.clock_sweep();
        assert!(s.enabled);
        assert_eq!(s.freq, 0x480);
        s.clock_sweep();
        assert!(!s.enabled);
    }

    #[test]
    fn sweep_overflow_is_checked_at_trigger() {
        let s = sweep_channel(0x01, 0x7FF); // shift 1, time 0: 0x7FF + 0x3FF
        assert!(!s.enabled);
        let s = sweep_channel(0x11, 0x600);
        assert!(!s.enabled);
    }

    #[test]
    fn sweep_time_zero_never_changes_the_frequency() {
        let mut s = sweep_channel(0x01, 0x100);
        for _ in 0..16 {
            s.clock_sweep();
        }
        assert_eq!(s.freq, 0x100);
    }

    #[test]
    fn sweep_runs_at_its_pace() {
        let mut s = sweep_channel(0x31, 0x100); // time 3
        s.clock_sweep();
        s.clock_sweep();
        assert_eq!(s.freq, 0x100);
        s.clock_sweep();
        assert_eq!(s.freq, 0x180);
    }

    #[test]
    fn channel_2_has_no_sweep() {
        let mut s = Square::new(false);
        s.write_nr10(0x11);
        s.write_envelope(0xF0);
        s.write_freq_lo(0x00);
        s.write_freq_hi_ctrl(0x81, false);
        for _ in 0..8 {
            s.clock_sweep();
        }
        assert_eq!(s.freq, 0x100);
    }

    #[test]
    fn reset_clears_everything() {
        let mut s = sweep_channel(0x11, 0x100);
        s.reset();
        assert!(!s.enabled);
        assert_eq!(s.freq, 0);
        assert_eq!(s.env.nr2, 0);
    }
}
