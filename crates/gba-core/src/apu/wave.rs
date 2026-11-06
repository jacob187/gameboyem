//! Wave channel 3: two 32-digit banks, 32- or 64-digit playback (GBATEK "GBA
//! Sound Channel 3 - Wave Output", SOUND3CNT_L/H/X and wave RAM).
//!
//! Adapted from `gb-core`'s `apu/wave.rs` (DECISIONS #60). Dropped: the DMG
//! retrigger wave-RAM corruption, the "only the byte being fetched is
//! reachable while playing" access window and the trigger delay (GBATEK
//! documents none of them for the GBA; wave RAM is plain memory here). Added:
//! the second bank, the dimension bit and the forced 75% volume.

use super::common::Length;
use crate::state::{StateError, check};
use emu_common::state::{StateReader, StateWriter};

/// CPU cycles per wave digit: the sample rate is `2097152 / (2048 - n)` Hz
/// (GBATEK SOUND3CNT_X), i.e. `8 * (2048 - n)` cycles at 16.78 MHz.
const DIGIT_CYCLES: u32 = 8;

#[derive(Clone, Debug)]
pub(super) struct Wave {
    pub enabled: bool,
    /// SOUND3CNT_L bit 7: playback on/off (the channel's "DAC").
    dac: bool,
    /// SOUND3CNT_L bit 5: 0 = one bank of 32 digits, 1 = two banks, 64 digits.
    two_banks: bool,
    /// SOUND3CNT_L bit 6: the bank that plays first; the CPU sees the other one.
    bank: u8,
    /// SOUND3CNT_H bits 13-14: 0 mute, 1 = 100%, 2 = 50%, 3 = 25%.
    level: u8,
    /// SOUND3CNT_H bit 15: force 75% whatever `level` says.
    force_75: bool,
    pub freq: u16,
    /// CPU cycles until the next digit.
    timer: u32,
    /// Index of the digit playing, 0..=31 (one bank) or 0..=63 (two banks).
    pos: u8,
    sample: u8,
    pub length: Length,
    /// Two banks of 16 bytes, high nibble first.
    pub ram: [[u8; 16]; 2],
}

impl Wave {
    pub fn new() -> Self {
        Self {
            enabled: false,
            dac: false,
            two_banks: false,
            bank: 0,
            level: 0,
            force_75: false,
            freq: 0,
            timer: DIGIT_CYCLES,
            pos: 0,
            sample: 0,
            length: Length::new(256),
            ram: [[0; 16]; 2],
        }
    }

    /// SOUNDCNT_X bit 7 cleared: the registers reset, wave RAM is kept.
    pub fn save_state(&self, w: &mut StateWriter) {
        w.bool(self.enabled);
        w.bool(self.dac);
        w.bool(self.two_banks);
        w.u8(self.bank);
        w.u8(self.level);
        w.bool(self.force_75);
        w.u16(self.freq);
        w.u32(self.timer);
        w.u8(self.pos);
        w.u8(self.sample);
        self.length.save_state(w);
        w.bytes(&self.ram[0]);
        w.bytes(&self.ram[1]);
    }

    /// `bank` picks one of two RAM banks, `pos` is a digit of a 32- or 64-digit
    /// pattern, `sample` a 4-bit digit, and the timer a countdown from one
    /// period (never zero).
    pub fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let mut next = Self::new();
        next.enabled = r.bool()?;
        next.dac = r.bool()?;
        next.two_banks = r.bool()?;
        next.bank = r.u8()?;
        next.level = r.u8()?;
        next.force_75 = r.bool()?;
        next.freq = r.u16()?;
        next.timer = r.u32()?;
        next.pos = r.u8()?;
        next.sample = r.u8()?;
        next.length.load_state(r)?;
        r.bytes_into(&mut next.ram[0])?;
        r.bytes_into(&mut next.ram[1])?;
        check(next.bank <= 1 && next.level <= 3, "wave bank or level")?;
        check(next.freq <= 2047, "wave frequency")?;
        check(
            (1..=DIGIT_CYCLES * 2048).contains(&next.timer),
            "wave timer",
        )?;
        check(next.pos < 64 && next.sample <= 15, "wave position")?;
        *self = next;
        Ok(())
    }

    pub fn reset(&mut self) {
        let ram = self.ram;
        *self = Self::new();
        self.ram = ram;
    }

    /// The bank the CPU reads and writes (the one that is not playing).
    pub fn cpu_bank(&self) -> usize {
        usize::from(self.bank ^ 1)
    }

    /// SOUND3CNT_L low byte.
    pub fn write_nr30(&mut self, v: u8) {
        self.two_banks = v & 0x20 != 0;
        self.bank = (v >> 6) & 1;
        self.dac = v & 0x80 != 0;
        if !self.dac {
            self.enabled = false;
        }
    }

    pub fn write_length(&mut self, v: u8) {
        self.length.load(v);
    }

    /// SOUND3CNT_H high byte.
    pub fn write_volume(&mut self, v: u8) {
        self.level = (v >> 5) & 3;
        self.force_75 = v & 0x80 != 0;
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
            // Output starts by replaying the selected bank from its first digit
            // (GBATEK SOUND3CNT_L).
            self.pos = 0;
            self.sample = self.fetch();
        }
    }

    fn period(&self) -> u32 {
        DIGIT_CYCLES * (2048 - u32::from(self.freq))
    }

    /// The digit at `pos`: high nibble of each byte plays first. In two-bank
    /// mode the selected bank plays digits 0..=31 and the other one 32..=63.
    fn fetch(&self) -> u8 {
        let bank = if self.two_banks {
            self.bank ^ (self.pos >> 5)
        } else {
            self.bank
        };
        let byte = self.ram[usize::from(bank)][usize::from((self.pos & 31) >> 1)];
        if self.pos & 1 == 0 {
            byte >> 4
        } else {
            byte & 0xF
        }
    }

    pub fn clock_length(&mut self) {
        if self.length.clock() {
            self.enabled = false;
        }
    }

    /// Cycles until the next digit, if the channel is running.
    pub fn next_step(&self) -> Option<u32> {
        self.enabled.then_some(self.timer)
    }

    /// Advance by `cycles` (never more than [`Wave::next_step`]); returns
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
        let len = if self.two_banks { 64 } else { 32 };
        self.pos = (self.pos + 1) % len;
        self.sample = self.fetch();
        self.output() != before
    }

    /// Digital output 0..=15.
    pub fn output(&self) -> u8 {
        if !self.enabled {
            return 0;
        }
        if self.force_75 {
            return self.sample * 3 / 4;
        }
        match self.level {
            0 => 0,
            1 => self.sample,
            2 => self.sample >> 1,
            _ => self.sample >> 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing(nr30: u8, level: u8, force: bool) -> Wave {
        let mut w = Wave::new();
        for i in 0..16 {
            w.ram[0][i] = (i as u8) << 4 | 0xF; // digits 0, F, 1, F, 2, F ...
            w.ram[1][i] = 0xA0 | (i as u8); // digits A, 0, A, 1, ...
        }
        w.write_nr30(nr30);
        w.write_volume(level << 5 | u8::from(force) << 7);
        w.write_freq_lo(0xFF);
        w.write_freq_hi_ctrl(0x87, false); // n = 2047: 8 cycles per digit
        w
    }

    fn digits(w: &mut Wave, n: usize) -> Vec<u8> {
        let mut out = vec![w.output()];
        for _ in 0..n - 1 {
            w.advance(w.next_step().unwrap());
            out.push(w.output());
        }
        out
    }

    #[test]
    fn digit_lasts_8_times_2048_minus_n_cycles() {
        let mut w = Wave::new();
        w.write_nr30(0x80);
        w.write_freq_lo(0x00);
        w.write_freq_hi_ctrl(0x84, false); // n = 1024
        assert_eq!(w.next_step(), Some(8 * 1024));
    }

    #[test]
    fn plays_high_nibble_first_and_wraps_at_32() {
        let mut w = playing(0x80, 1, false);
        let d = digits(&mut w, 34);
        assert_eq!(&d[..6], &[0, 0xF, 1, 0xF, 2, 0xF]);
        assert_eq!(d[32], d[0], "wraps after 32 digits");
        assert_eq!(d[33], d[1]);
    }

    #[test]
    fn bank_select_chooses_the_playing_bank_and_the_cpu_sees_the_other() {
        let mut w = playing(0x80 | 0x40, 1, false);
        assert_eq!(w.cpu_bank(), 0);
        assert_eq!(digits(&mut w, 4), [0xA, 0, 0xA, 1]);
        w.write_nr30(0x80);
        assert_eq!(w.cpu_bank(), 1);
    }

    #[test]
    fn dimension_mode_plays_both_banks_selected_first() {
        let mut w = playing(0x80 | 0x20, 1, false); // two banks, bank 0 first
        let d = digits(&mut w, 66);
        assert_eq!(&d[..4], &[0, 0xF, 1, 0xF]);
        assert_eq!(&d[32..36], &[0xA, 0, 0xA, 1], "then bank 1");
        assert_eq!(&d[64..66], &[0, 0xF], "then back to bank 0");

        let mut w = playing(0x80 | 0x20 | 0x40, 1, false); // bank 1 first
        let d = digits(&mut w, 36);
        assert_eq!(&d[..4], &[0xA, 0, 0xA, 1]);
        assert_eq!(&d[32..36], &[0, 0xF, 1, 0xF]);
    }

    #[test]
    fn volume_levels() {
        // Digit 1 of bank 0 is 0xF.
        let level = |lvl, force| {
            let mut w = playing(0x80, lvl, force);
            w.advance(w.next_step().unwrap());
            w.output()
        };
        assert_eq!(level(0, false), 0);
        assert_eq!(level(1, false), 15);
        assert_eq!(level(2, false), 7);
        assert_eq!(level(3, false), 3);
        assert_eq!(level(0, true), 11, "forced 75% ignores the volume bits");
        assert_eq!(level(1, true), 11);
        assert_eq!(level(3, true), 11);
    }

    #[test]
    fn playback_bit_off_stops_the_channel() {
        let mut w = playing(0x80, 1, false);
        assert!(w.enabled);
        w.write_nr30(0x00);
        assert!(!w.enabled);
        w.write_freq_hi_ctrl(0x80, false);
        assert!(!w.enabled, "cannot restart with playback off");
    }

    #[test]
    fn length_counts_256_minus_n() {
        let mut w = Wave::new();
        w.write_nr30(0x80);
        w.write_length(254);
        w.write_freq_hi_ctrl(0xC0, false);
        w.clock_length();
        assert!(w.enabled);
        w.clock_length();
        assert!(!w.enabled);
    }

    #[test]
    fn reset_keeps_wave_ram() {
        let mut w = playing(0x80, 1, false);
        w.reset();
        assert!(!w.enabled);
        assert_eq!(w.ram[1][3], 0xA3);
    }
}
