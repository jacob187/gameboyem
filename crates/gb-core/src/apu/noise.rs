//! Noise channel CH4 (Pan Docs: "Audio Registers", NR41-NR44, "Audio Details").

use super::common::{Envelope, Length};

#[derive(Clone, Debug)]
pub(super) struct Noise {
    pub enabled: bool,
    /// Raw NR43: clock shift (7-4), width (3, 1 = 7-bit), divisor code (2-0).
    nr43: u8,
    /// 15-bit LFSR. Pan Docs: reset to 0 on trigger; the new bit is the XNOR of
    /// bits 0 and 1 and the output is high when bit 0 is set.
    pub lfsr: u16,
    /// T-cycles until the next LFSR clock.
    timer: i32,
    pub length: Length,
    pub env: Envelope,
}

impl Noise {
    pub fn new() -> Self {
        Self {
            enabled: false,
            nr43: 0,
            lfsr: 0,
            timer: 8,
            length: Length::new(64),
            env: Envelope::default(),
        }
    }

    pub fn power_off(&mut self) {
        self.enabled = false;
        self.nr43 = 0;
        self.length.enabled = false;
        self.env.nr2 = 0;
        self.env.volume = 0;
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
            self.lfsr = 0;
            self.timer = self.period();
            self.env.trigger();
        }
    }

    fn shift(&self) -> u8 {
        self.nr43 >> 4
    }

    /// LFSR clock period in T-cycles: divisor (8, 16, 32, ... 112) << shift.
    fn period(&self) -> i32 {
        let r = i32::from(self.nr43 & 7);
        let divisor = if r == 0 { 8 } else { 16 * r };
        divisor << self.shift()
    }

    pub fn clock_lfsr(&mut self) {
        let l = self.lfsr;
        let new = u16::from(l & 1 == (l >> 1) & 1); // XNOR
        let mut l = (l >> 1) & !(1 << 14) | new << 14;
        if self.nr43 & 8 != 0 {
            l = l & !(1 << 6) | new << 6; // 7-bit mode copies the bit into bit 6
        }
        self.lfsr = l;
    }

    pub fn clock_length(&mut self) {
        if self.length.clock() {
            self.enabled = false;
        }
    }

    pub fn clock_envelope(&mut self) {
        self.env.clock();
    }

    /// Advance one M-cycle; returns `true` if the output level changed.
    pub fn tick_mcycle(&mut self) -> bool {
        // Shifts 14 and 15 never clock the LFSR (Pan Docs: CH4).
        if !self.enabled || self.shift() >= 14 {
            return false;
        }
        self.timer -= 4;
        if self.timer > 0 {
            return false;
        }
        let before = self.output();
        while self.timer <= 0 {
            self.timer += self.period();
            self.clock_lfsr();
        }
        self.output() != before
    }

    /// Digital output 0..=15.
    pub fn output(&self) -> u8 {
        if self.enabled && self.lfsr & 1 != 0 {
            self.env.volume
        } else {
            0
        }
    }
}
