//! Wave channel CH3 (Pan Docs: "Audio Registers", NR30-NR34, "Wave Pattern RAM").

use super::common::Length;

#[derive(Clone, Debug)]
pub(super) struct Wave {
    pub enabled: bool,
    dac: bool,
    /// NR32 bits 6-5: 0 mute, 1 = 100%, 2 = 50%, 3 = 25%.
    level: u8,
    freq: u16,
    /// T-cycles until the next sample fetch. Kept in T-cycles because the
    /// period `(2048 - freq) * 2` can be shorter than one M-cycle.
    timer: i32,
    /// Index of the sample (nibble) fetched last, 0..=31.
    pos: u8,
    /// Last fetched nibble. Not cleared by a trigger (Pan Docs: CH3 trigger).
    sample: u8,
    /// A sample fetch happened during the current M-cycle (see `ram_index`).
    just_read: bool,
    pub length: Length,
    pub ram: [u8; 0x10],
}

impl Wave {
    pub fn new() -> Self {
        Self {
            enabled: false,
            dac: false,
            level: 0,
            freq: 0,
            timer: 4,
            pos: 0,
            sample: 0,
            just_read: false,
            length: Length::new(256),
            ram: [0; 0x10],
        }
    }

    pub fn power_off(&mut self) {
        self.enabled = false;
        self.dac = false;
        self.level = 0;
        self.freq = 0;
        self.length.enabled = false;
    }

    /// Power-on clears the sample buffer (Pan Docs: CH3).
    pub fn power_on(&mut self) {
        self.sample = 0;
    }

    pub fn write_nr30(&mut self, v: u8) {
        self.dac = v & 0x80 != 0;
        if !self.dac {
            self.enabled = false;
        }
    }

    pub fn write_length(&mut self, v: u8) {
        self.length.load(v);
    }

    pub fn write_level(&mut self, v: u8) {
        self.level = (v >> 5) & 3;
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
            self.length.trigger(next_step_odd);
            self.enabled = self.dac;
            self.timer = self.period();
            self.pos = 0;
        }
    }

    fn period(&self) -> i32 {
        (2048 - i32::from(self.freq)) * 2
    }

    pub fn clock_length(&mut self) {
        if self.length.clock() {
            self.enabled = false;
        }
    }

    /// Advance one M-cycle; returns `true` if the output level changed.
    pub fn tick_mcycle(&mut self) -> bool {
        self.just_read = false;
        if !self.enabled {
            return false;
        }
        let before = self.output();
        self.timer -= 4;
        while self.timer <= 0 {
            self.timer += self.period();
            self.pos = (self.pos + 1) & 31;
            let byte = self.ram[usize::from(self.pos >> 1)];
            // High nibble plays first (Pan Docs: "Wave Pattern RAM").
            self.sample = if self.pos & 1 == 0 {
                byte >> 4
            } else {
                byte & 0xF
            };
            self.just_read = true;
        }
        self.output() != before
    }

    /// Digital output 0..=15.
    pub fn output(&self) -> u8 {
        if !self.enabled {
            return 0;
        }
        match self.level {
            0 => 0,
            1 => self.sample,
            2 => self.sample >> 1,
            _ => self.sample >> 2,
        }
    }

    /// Wave RAM byte reached by a CPU access. While the channel plays, DMG
    /// exposes only the byte being fetched, and only in the M-cycle of the
    /// fetch; otherwise reads give $FF and writes are lost (Pan Docs: "Wave
    /// Pattern RAM"). Timing is approximate: M-cycle rather than T-cycle.
    pub fn ram_index(&self, offset: usize) -> Option<usize> {
        if !self.enabled {
            Some(offset)
        } else if self.just_read {
            Some(usize::from(self.pos >> 1))
        } else {
            None
        }
    }
}
