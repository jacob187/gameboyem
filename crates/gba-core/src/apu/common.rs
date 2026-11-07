//! Building blocks shared by the PSG channels: length counter and volume
//! envelope.
//!
//! Adapted from `gb-core`'s `apu/common.rs` (DECISIONS #61). Added: save-state
//! code, with range checks on load. Dropped: the envelope "zombie mode" volume
//! glitch on NRx2 writes and the
//! "trigger just before the envelope step" timer bonus (DMG-specific, Pan Docs
//! "Obscure Behavior"). GBATEK "GBA Sound Channel 1" only specifies the plain
//! decay/grow behaviour below.

use crate::state::{StateError, check};
use emu_common::state::{StateReader, StateWriter};

/// A 3-bit period field (envelope step time, sweep time) as a reload value:
/// 0 counts as 8 so the timer still runs, though the unit never steps.
pub(super) fn period_or_8(bits: u8) -> u8 {
    match bits & 7 {
        0 => 8,
        p => p,
    }
}

/// Length counter. Clocked at 256 Hz by the frame sequencer; when it reaches
/// zero with length enabled, the owning channel is switched off. Lengths are
/// `(max - n) / 256` seconds (GBATEK "Sound Channel 1/3/4", length bits).
#[derive(Clone, Debug)]
pub(super) struct Length {
    pub counter: u16,
    pub enabled: bool,
    max: u16,
}

impl Length {
    pub fn new(max: u16) -> Self {
        Self {
            counter: 0,
            enabled: false,
            max,
        }
    }

    pub fn save_state(&self, w: &mut StateWriter) {
        w.u16(self.counter);
        w.bool(self.enabled);
    }

    /// The counter never exceeds the channel's `max` (64 or 256): it is loaded as
    /// `max - value`, decremented, or reloaded to `max`.
    pub fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let counter = r.u16()?;
        let enabled = r.bool()?;
        check(counter <= self.max, "length counter")?;
        self.counter = counter;
        self.enabled = enabled;
        Ok(())
    }

    /// Length write: the counter counts down from `max - value`.
    pub fn load(&mut self, value: u8) {
        self.counter = self.max - u16::from(value);
    }

    /// Frame-sequencer clock. Returns `true` if the channel must be disabled.
    pub fn clock(&mut self) -> bool {
        if self.enabled && self.counter > 0 {
            self.counter -= 1;
            self.counter == 0
        } else {
            false
        }
    }

    /// Length-flag write. Same quirk as the GB family (Pan Docs "Audio Details",
    /// length counter quirks; the AGB PSG is the CGB's): enabling length while the
    /// next frame-sequencer step does not clock length gives one extra clock.
    /// Returns `true` if that expired the counter.
    pub fn set_enabled(&mut self, enable: bool, next_step_odd: bool) -> bool {
        let was = self.enabled;
        self.enabled = enable;
        if !was && enable && next_step_odd {
            self.clock()
        } else {
            false
        }
    }

    /// Trigger: a zero counter reloads to `max` (one less if length is enabled
    /// and the next step does not clock length).
    pub fn trigger(&mut self, next_step_odd: bool) {
        if self.counter == 0 {
            self.counter = self.max;
            if self.enabled && next_step_odd {
                self.counter -= 1;
            }
        }
    }
}

/// Volume envelope. Clocked at 64 Hz by the frame sequencer; the step time is
/// `n / 64` s, 0 meaning no envelope (GBATEK "Sound Channel 1", SOUND1CNT_H).
#[derive(Clone, Debug, Default)]
pub(super) struct Envelope {
    /// Raw NRx2 byte: initial volume (7-4), direction (3, 1 = increase), step
    /// time (2-0).
    pub nr2: u8,
    pub volume: u8,
    timer: u8,
    /// Automatic updates are running: set by a trigger, cleared when a clock
    /// tries to step the volume past 0 or 15.
    auto: bool,
}

impl Envelope {
    pub fn save_state(&self, w: &mut StateWriter) {
        w.u8(self.nr2);
        w.u8(self.volume);
        w.u8(self.timer);
        w.bool(self.auto);
    }

    /// The volume is a 4-bit level and the timer a 3-bit period (1..=8).
    pub fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let (nr2, volume, timer, auto) = (r.u8()?, r.u8()?, r.u8()?, r.bool()?);
        check(volume <= 15, "envelope volume")?;
        check(timer <= 8, "envelope timer")?;
        *self = Self {
            nr2,
            volume,
            timer,
            auto,
        };
        Ok(())
    }

    /// The channel's DAC is on iff NRx2 bits 7-3 are not all zero (the GB family
    /// rule; GBATEK is silent, and the AGB PSG is the same circuit).
    pub fn dac_on(&self) -> bool {
        self.nr2 & 0xF8 != 0
    }

    pub fn trigger(&mut self) {
        self.volume = self.nr2 >> 4;
        self.timer = period_or_8(self.nr2);
        self.auto = true;
    }

    pub fn clock(&mut self) {
        if !self.auto {
            return;
        }
        self.timer = self.timer.saturating_sub(1);
        if self.timer != 0 {
            return;
        }
        self.timer = period_or_8(self.nr2);
        if self.nr2 & 7 == 0 {
            return; // step time 0: no envelope
        }
        let v = if self.nr2 & 8 != 0 {
            self.volume + 1
        } else {
            self.volume.wrapping_sub(1)
        };
        if v > 15 {
            self.auto = false; // 15 + 1 and 0 - 1 (wrapped) both land here
        } else {
            self.volume = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_counts_down_and_expires() {
        let mut l = Length::new(64);
        l.load(62);
        assert_eq!(l.counter, 2);
        l.enabled = true;
        assert!(!l.clock());
        assert!(l.clock(), "second clock expires");
        assert!(!l.clock(), "stays expired");
    }

    #[test]
    fn disabled_length_never_expires() {
        let mut l = Length::new(64);
        l.load(63);
        assert!(!l.clock());
        assert_eq!(l.counter, 1);
    }

    #[test]
    fn envelope_decays_by_one_per_step_time() {
        let mut e = Envelope {
            nr2: 0x72, // volume 7, decrease, step time 2
            ..Default::default()
        };
        e.trigger();
        assert_eq!(e.volume, 7);
        e.clock();
        assert_eq!(e.volume, 7);
        e.clock();
        assert_eq!(e.volume, 6, "one step every 2 clocks");
        for _ in 0..40 {
            e.clock();
        }
        assert_eq!(e.volume, 0, "stops at 0");
    }

    #[test]
    fn envelope_grows_and_stops_at_15() {
        let mut e = Envelope {
            nr2: 0xD9, // volume 13, increase, step time 1
            ..Default::default()
        };
        e.trigger();
        e.clock();
        e.clock();
        e.clock();
        e.clock();
        assert_eq!(e.volume, 15);
    }

    #[test]
    fn step_time_zero_means_no_envelope() {
        let mut e = Envelope {
            nr2: 0x80,
            ..Default::default()
        };
        e.trigger();
        for _ in 0..32 {
            e.clock();
        }
        assert_eq!(e.volume, 8);
    }

    #[test]
    fn dac_needs_volume_or_increase() {
        let dac = |nr2| {
            Envelope {
                nr2,
                ..Default::default()
            }
            .dac_on()
        };
        assert!(!dac(0x00));
        assert!(!dac(0x07), "step time alone does not power the DAC");
        assert!(dac(0x08), "volume 0 but increasing");
        assert!(dac(0x10));
    }

    /// Length enable on a step whose successor does not clock length adds one
    /// clock, and only on a rising edge of the flag (deep-review #73).
    #[test]
    fn enabling_length_before_an_odd_step_clocks_it_once() {
        let loaded = || {
            let mut l = Length::new(64);
            l.load(62);
            l
        };
        let mut l = loaded();
        assert!(!l.set_enabled(true, true));
        assert_eq!(l.counter, 1, "extra clock");
        // Already enabled: no edge, no extra clock.
        assert!(!l.set_enabled(true, true));
        assert_eq!(l.counter, 1);
        // The extra clock can expire the channel.
        let mut l = Length::new(64);
        l.load(63);
        assert!(l.set_enabled(true, true));
        // On an even step, or leaving the flag off, nothing extra happens.
        let mut l = loaded();
        assert!(!l.set_enabled(true, false));
        assert_eq!(l.counter, 2);
        let mut l = loaded();
        assert!(!l.set_enabled(false, true));
        assert_eq!(l.counter, 2);
    }

    /// A trigger on an expired counter reloads it to `max`, one less when length
    /// is enabled and the next step is odd (deep-review #73).
    #[test]
    fn triggering_an_expired_length_before_an_odd_step_loses_one() {
        for (enabled, odd, want) in [
            (true, true, 63),
            (true, false, 64),
            (false, true, 64),
            (false, false, 64),
        ] {
            let mut l = Length::new(64);
            l.enabled = enabled;
            l.trigger(odd);
            assert_eq!(l.counter, want, "enabled {enabled}, odd {odd}");
        }
        // A running counter is left alone.
        let mut l = Length::new(64);
        l.load(10);
        l.enabled = true;
        l.trigger(true);
        assert_eq!(l.counter, 54);
    }
}
