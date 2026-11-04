//! Building blocks shared by the sound channels: length counter and volume
//! envelope (Pan Docs: "Audio Details").

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
/// Gaps (documented in `apu.rs`): the trigger-on-envelope-step timing quirk and
/// DMG "zombie mode" writes to NRx2 while the channel plays are not modelled.
#[derive(Clone, Debug, Default)]
pub(super) struct Envelope {
    /// Raw NRx2: initial volume (7-4), direction (3, 1 = increase), pace (2-0).
    pub nr2: u8,
    pub volume: u8,
    timer: u8,
}

impl Envelope {
    /// The channel's DAC is on iff NRx2 bits 7-3 are not all zero.
    pub fn dac_on(&self) -> bool {
        self.nr2 & 0xF8 != 0
    }

    pub fn trigger(&mut self) {
        self.volume = self.nr2 >> 4;
        self.timer = self.pace_or_8();
    }

    fn pace_or_8(&self) -> u8 {
        match self.nr2 & 7 {
            0 => 8,
            p => p,
        }
    }

    pub fn clock(&mut self) {
        let pace = self.nr2 & 7;
        if pace == 0 {
            return; // pace 0 disables the envelope (Pan Docs: NRx2)
        }
        self.timer = self.timer.saturating_sub(1);
        if self.timer == 0 {
            self.timer = pace;
            if self.nr2 & 8 != 0 {
                self.volume = (self.volume + 1).min(15);
            } else {
                self.volume = self.volume.saturating_sub(1);
            }
        }
    }
}
