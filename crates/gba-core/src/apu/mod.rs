//! Sound: four PSG channels, Direct Sound FIFO A/B, and the SOUNDCNT/SOUNDBIAS
//! mixer. Built in phase P2 (P2-APU). **Contract** for the bus: FIFO writes
//! (0x0400_00A0 / 0x0400_00A4) and [`Apu::on_timer_overflow`].

/// Which FIFOs want a DMA refill after a timer overflow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FifoRequest {
    pub a: bool,
    pub b: bool,
}

/// Default host sample rate until the frontend sets its device rate.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;

#[derive(Debug, Clone)]
pub struct Apu {
    sample_rate: u32,
}

impl Default for Apu {
    fn default() -> Self {
        Self {
            sample_rate: DEFAULT_SAMPLE_RATE,
        }
    }
}

impl Apu {
    /// Timer `n` (0 or 1) overflowed: FIFOs clocked by it pop a sample, and any
    /// that fall to 16 bytes or fewer request a refill. **Contract.**
    pub fn on_timer_overflow(&mut self, n: usize) -> FifoRequest {
        let _ = n;
        FifoRequest::default()
    }

    /// A 32-bit write to FIFO A (`b = false`) or B. **Contract.**
    pub fn write_fifo(&mut self, b: bool, value: u32) {
        let _ = (b, value);
    }

    /// Append the stereo samples produced since the last call, interleaved
    /// `[l, r, l, r, ...]` in -1.0..=1.0 at [`Apu::sample_rate`], the same
    /// format as `gb-core`. **Contract** (P2-APU fills it in; empty until then).
    pub fn drain_samples(&mut self, out: &mut Vec<f32>) {
        let _ = out;
    }

    /// Host output rate in Hz. **Contract.**
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Set the host output rate (clamped to 8 kHz..=192 kHz). **Contract.**
    pub fn set_sample_rate(&mut self, hz: u32) {
        self.sample_rate = hz.clamp(8_000, 192_000);
    }
}
