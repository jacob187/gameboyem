//! Pulse channels CH1 (with sweep) and CH2 (Pan Docs: "Audio Registers",
//! NR10-NR14 / NR21-NR24).

use super::common::{Envelope, Length};

/// Duty waveforms, one bit per step, first step first (Pan Docs: NRx1).
const DUTY: [[u8; 8]; 4] = [
    [0, 0, 0, 0, 0, 0, 0, 1], // 12.5%
    [1, 0, 0, 0, 0, 0, 0, 1], // 25%
    [1, 0, 0, 0, 0, 1, 1, 1], // 50%
    [0, 1, 1, 1, 1, 1, 1, 0], // 75%
];

#[derive(Clone, Debug, Default)]
struct Sweep {
    /// Raw NR10: pace (6-4), direction (3, 1 = subtract), shift (2-0).
    nr10: u8,
    shadow: u16,
    timer: u8,
    enabled: bool,
    /// A subtraction calculation happened since the last trigger.
    negate_used: bool,
}

impl Sweep {
    fn pace(&self) -> u8 {
        (self.nr10 >> 4) & 7
    }

    fn pace_or_8(&self) -> u8 {
        match self.pace() {
            0 => 8,
            p => p,
        }
    }

    fn shift(&self) -> u8 {
        self.nr10 & 7
    }

    /// Next frequency from the shadow register (Pan Docs: "Audio Details",
    /// sweep). May exceed 2047, which the caller treats as overflow.
    fn calculate(&mut self) -> u16 {
        let delta = self.shadow >> self.shift();
        if self.nr10 & 8 != 0 {
            self.negate_used = true;
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
    /// M-cycles until the next duty step (a step lasts `2048 - freq` M-cycles).
    timer: u16,
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
            timer: 1,
            length: Length::new(64),
            env: Envelope::default(),
            sweep: Sweep::default(),
        }
    }

    /// Return to the NR52-off state. Length counters survive on DMG.
    pub fn power_off(&mut self) {
        self.enabled = false;
        self.duty = 0;
        self.pos = 0;
        self.freq = 0;
        self.length.enabled = false;
        self.env.nr2 = 0;
        self.env.volume = 0;
        self.sweep = Sweep::default();
    }

    /// Power-on resets the duty step (Pan Docs: "Audio Details").
    pub fn power_on(&mut self) {
        self.pos = 0;
    }

    pub fn write_nr10(&mut self, v: u8) {
        // Clearing the negate bit after a subtraction since the last trigger
        // disables the channel at once (Pan Docs: sweep quirks).
        if self.sweep.negate_used && v & 8 == 0 {
            self.enabled = false;
        }
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
        self.timer = 2048 - self.freq;
        self.env.trigger();
        if self.has_sweep {
            let s = &mut self.sweep;
            s.shadow = self.freq;
            s.timer = s.pace_or_8();
            s.enabled = s.pace() != 0 || s.shift() != 0;
            s.negate_used = false;
            if s.shift() != 0 && s.calculate() > 2047 {
                self.enabled = false;
            }
        }
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
        s.timer = s.pace_or_8();
        if !s.enabled || s.pace() == 0 {
            return;
        }
        let new = s.calculate();
        if new > 2047 {
            self.enabled = false;
        } else if s.shift() != 0 {
            s.shadow = new;
            self.freq = new;
            // Second calculation checks overflow only; result is discarded.
            if s.calculate() > 2047 {
                self.enabled = false;
            }
        }
    }

    /// Advance one M-cycle; returns `true` if the output level changed.
    pub fn tick_mcycle(&mut self) -> bool {
        if !self.enabled {
            return false;
        }
        self.timer -= 1;
        if self.timer != 0 {
            return false;
        }
        self.timer = 2048 - self.freq;
        let before = self.output();
        self.pos = (self.pos + 1) & 7;
        // Volume 0 is silent whatever the duty step, so nothing changed.
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
