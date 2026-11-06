//! Noise channel 4 (GBATEK "GBA Sound Channel 4 - Noise", SOUND4CNT_L/H).
//!
//! Adapted from `gb-core`'s `apu/noise.rs` (DECISIONS #61). The LFSR is
//! GBATEK's, not the DMG's XNOR form: shift right, and if the bit shifted out
//! was 1 output HIGH and XOR the register with 0x60 (7-bit) or 0x6000 (15-bit);
//! the seed on restart is 0x40 / 0x4000.

use super::common::{Envelope, Length};
use crate::state::{StateError, check};
use emu_common::state::{StateReader, StateWriter};

/// The longest LFSR period: ratio 7 (448 cycles) at shift clock 15.
const MAX_PERIOD: u32 = 448 << 15;

#[derive(Clone, Debug)]
pub(super) struct Noise {
    pub enabled: bool,
    /// Raw SOUND4CNT_H low byte: shift clock frequency `s` (7-4), width (3,
    /// 1 = 7-bit), dividing ratio `r` (2-0).
    nr43: u8,
    pub lfsr: u16,
    /// Output of the last LFSR step (HIGH = the shifted-out bit was 1).
    high: bool,
    /// CPU cycles until the next LFSR step.
    timer: u32,
    pub length: Length,
    pub env: Envelope,
}

impl Noise {
    pub fn new() -> Self {
        Self {
            enabled: false,
            nr43: 0,
            lfsr: 0x4000,
            high: false,
            timer: 32,
            length: Length::new(64),
            env: Envelope::default(),
        }
    }

    pub fn save_state(&self, w: &mut StateWriter) {
        w.bool(self.enabled);
        w.u8(self.nr43);
        w.u16(self.lfsr);
        w.bool(self.high);
        w.u32(self.timer);
        self.length.save_state(w);
        self.env.save_state(w);
    }

    /// The timer counts down from at most the longest period and is never zero.
    pub fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let mut next = Self::new();
        next.enabled = r.bool()?;
        next.nr43 = r.u8()?;
        next.lfsr = r.u16()?;
        next.high = r.bool()?;
        next.timer = r.u32()?;
        next.length.load_state(r)?;
        next.env.load_state(r)?;
        check((1..=MAX_PERIOD).contains(&next.timer), "noise timer")?;
        *self = next;
        Ok(())
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn write_length(&mut self, v: u8) {
        self.length.load(v & 0x3F);
    }

    pub fn write_envelope(&mut self, v: u8) {
        self.env.nr2 = v;
        if !self.env.dac_on() {
            self.enabled = false;
        }
    }

    pub fn write_nr43(&mut self, v: u8) {
        self.nr43 = v;
    }

    pub fn write_ctrl(&mut self, v: u8, next_step_odd: bool) {
        if self.length.set_enabled(v & 0x40 != 0, next_step_odd) {
            self.enabled = false;
        }
        if v & 0x80 != 0 {
            self.length.trigger(next_step_odd);
            self.enabled = self.env.dac_on();
            self.lfsr = if self.seven_bit() { 0x40 } else { 0x4000 };
            self.high = false;
            self.timer = self.period();
            self.env.trigger();
        }
    }

    fn seven_bit(&self) -> bool {
        self.nr43 & 8 != 0
    }

    /// CPU cycles per LFSR step: the frequency is `524288 / r / 2^(s+1)` Hz with
    /// r = 0.5 for 0 (GBATEK SOUND4CNT_H), so `64 * r * 2^s` cycles, `32 * 2^s`
    /// for r = 0.
    fn period(&self) -> u32 {
        let r = u32::from(self.nr43 & 7);
        let base = if r == 0 { 32 } else { 64 * r };
        base << (self.nr43 >> 4)
    }

    fn step_lfsr(&mut self) {
        let carry = self.lfsr & 1 != 0;
        self.lfsr >>= 1;
        if carry {
            self.lfsr ^= if self.seven_bit() { 0x60 } else { 0x6000 };
        }
        self.high = carry;
    }

    pub fn clock_length(&mut self) {
        if self.length.clock() {
            self.enabled = false;
        }
    }

    pub fn clock_envelope(&mut self) {
        self.env.clock();
    }

    /// Cycles until the next LFSR step, if the channel is running.
    pub fn next_step(&self) -> Option<u32> {
        self.enabled.then_some(self.timer)
    }

    /// Advance by `cycles` (never more than [`Noise::next_step`]); returns
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
        let before = self.high;
        self.step_lfsr();
        self.env.volume != 0 && self.high != before
    }

    /// Digital output 0..=15.
    pub fn output(&self) -> u8 {
        if self.enabled && self.high {
            self.env.volume
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(nr43: u8) -> Noise {
        let mut n = Noise::new();
        n.write_envelope(0xF0);
        n.write_nr43(nr43);
        n.write_ctrl(0x80, false);
        n
    }

    /// Steps until the LFSR returns to its seed.
    fn period_of(nr43: u8) -> usize {
        let mut n = started(nr43);
        let seed = n.lfsr;
        let mut steps = 0;
        loop {
            n.step_lfsr();
            steps += 1;
            if n.lfsr == seed {
                return steps;
            }
            assert!(steps < 100_000);
        }
    }

    #[test]
    fn lfsr_period_is_127_in_7_bit_mode_and_32767_in_15_bit_mode() {
        assert_eq!(period_of(0x08), 127);
        assert_eq!(period_of(0x00), 32767);
    }

    #[test]
    fn restart_seeds_per_width() {
        assert_eq!(started(0x08).lfsr, 0x40);
        assert_eq!(started(0x00).lfsr, 0x4000);
    }

    #[test]
    fn first_steps_follow_the_shift_xor_rule() {
        let mut n = started(0x08);
        // 0x40 >> 1 = 0x20, carry 0: LOW.
        n.step_lfsr();
        assert_eq!((n.lfsr, n.high), (0x20, false));
        for _ in 0..5 {
            n.step_lfsr();
        }
        assert_eq!(n.lfsr, 0x01);
        n.step_lfsr();
        // 0x01 >> 1 = 0 with carry: XOR 0x60, HIGH.
        assert_eq!((n.lfsr, n.high), (0x60, true));
        n.step_lfsr();
        assert_eq!((n.lfsr, n.high), (0x30, false));
    }

    #[test]
    fn seven_bit_state_stays_in_seven_bits() {
        let mut n = started(0x08);
        for _ in 0..300 {
            n.step_lfsr();
            assert!(n.lfsr < 0x80);
        }
    }

    #[test]
    fn period_formula() {
        let p = |nr43| started(nr43).next_step().unwrap();
        assert_eq!(p(0x00), 32, "r = 0 counts as 0.5");
        assert_eq!(p(0x01), 64);
        assert_eq!(p(0x07), 448);
        assert_eq!(p(0x30), 32 << 3);
        assert_eq!(p(0x52), 128 << 5);
    }

    #[test]
    fn output_is_the_envelope_volume_when_high() {
        let mut n = started(0x00);
        assert_eq!(n.output(), 0, "LOW until the first HIGH step");
        let mut seen = [false; 2];
        for _ in 0..64 {
            n.advance(n.next_step().unwrap());
            seen[usize::from(n.output() != 0)] = true;
            assert!(n.output() == 0 || n.output() == 15);
        }
        assert_eq!(seen, [true, true]);
    }

    #[test]
    fn length_and_dac() {
        let mut n = Noise::new();
        n.write_envelope(0x00);
        n.write_ctrl(0x80, false);
        assert!(!n.enabled);
        n.write_envelope(0xF0);
        n.write_length(62);
        n.write_ctrl(0xC0, false);
        n.clock_length();
        n.clock_length();
        assert!(!n.enabled);
    }
}
