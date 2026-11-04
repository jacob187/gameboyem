//! Building blocks shared by the sound channels: length counter and volume
//! envelope (Pan Docs: "Audio Details").

/// A 3-bit period field (envelope pace, sweep pace) as a timer reload value:
/// 0 counts as 8.
pub(super) fn period_or_8(bits: u8) -> u8 {
    match bits & 7 {
        0 => 8,
        p => p,
    }
}

/// Length counter. Clocked at 256 Hz by the frame sequencer; when it reaches
/// zero with length enabled, the owning channel is switched off.
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

    /// NRx1 length write: the counter counts up from the written value to `max`.
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

    /// NRx4 bit 6 write. Pan Docs ("Audio Details", length counter quirks): if
    /// length was disabled, is now enabled, and the *next* frame-sequencer step
    /// does not clock length, the counter gets one extra clock. Returns `true`
    /// if that extra clock expired the counter.
    pub fn set_enabled(&mut self, enable: bool, next_step_odd: bool) -> bool {
        let was = self.enabled;
        self.enabled = enable;
        if !was && enable && next_step_odd {
            self.clock()
        } else {
            false
        }
    }

    /// Trigger: a zero counter reloads to `max`; if length is enabled and the
    /// next step does not clock length, it reloads to `max - 1` (Pan Docs: same
    /// quirks section).
    pub fn trigger(&mut self, next_step_odd: bool) {
        if self.counter == 0 {
            self.counter = self.max;
            if self.enabled && next_step_odd {
                self.counter -= 1;
            }
        }
    }
}

/// Volume envelope (NRx2). Clocked at 64 Hz by the frame sequencer.
///
/// Behaviours beyond the basic decay/grow (Pan Docs: "Audio Details",
/// "Obscure Behavior"):
/// - a period of 0 is treated as 8 for the timer, but never changes the volume;
/// - a trigger that lands just before the sequencer step which clocks the
///   envelope reloads the timer one higher than normal;
/// - writing NRx2 while the channel plays ("zombie mode") alters the volume
///   directly, see [`Envelope::write`].
#[derive(Clone, Debug, Default)]
pub(super) struct Envelope {
    /// Raw NRx2: initial volume (7-4), direction (3, 1 = increase), pace (2-0).
    pub nr2: u8,
    pub volume: u8,
    timer: u8,
    /// Automatic updates are running: set by a trigger, cleared when a clock
    /// tries to step the volume past 0 or 15.
    auto: bool,
}

impl Envelope {
    /// The channel's DAC is on iff NRx2 bits 7-3 are not all zero.
    pub fn dac_on(&self) -> bool {
        self.nr2 & 0xF8 != 0
    }

    /// `env_next`: the next frame-sequencer step is the one that clocks the
    /// envelope (step 7).
    pub fn trigger(&mut self, env_next: bool) {
        self.volume = self.nr2 >> 4;
        self.timer = self.pace_or_8() + u8::from(env_next);
        self.auto = true;
    }

    fn pace_or_8(&self) -> u8 {
        period_or_8(self.nr2)
    }

    /// NRx2 write. While the channel is on (`channel_on`) the volume changes as
    /// on the DMG family (Pan Docs: "Zombie mode"): +1 if the old period was 0
    /// and the envelope still auto-updates, otherwise +2 if the old direction
    /// was decrease; then `16 - volume` if the direction flipped; only the low
    /// 4 bits are kept. The timer is not reloaded.
    pub fn write(&mut self, v: u8, channel_on: bool) {
        if channel_on {
            let old = self.nr2;
            let mut vol = self.volume;
            if old & 7 == 0 && self.auto {
                vol += 1;
            } else if old & 8 == 0 {
                vol += 2;
            }
            if (old ^ v) & 8 != 0 {
                vol = 16u8.wrapping_sub(vol);
            }
            self.volume = vol & 0xF;
        }
        self.nr2 = v;
    }

    pub fn clock(&mut self) {
        if !self.auto {
            return;
        }
        self.timer = self.timer.saturating_sub(1);
        if self.timer != 0 {
            return;
        }
        self.timer = self.pace_or_8();
        if self.nr2 & 7 == 0 {
            return; // pace 0 reloads the timer but never steps the volume
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
