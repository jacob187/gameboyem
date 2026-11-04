//! Audio processing unit (Pan Docs: "Audio", "Audio Registers").
//!
//! Phase 1 stub: register storage with correct read-back masks and a silent,
//! correctly-paced stereo sample stream. Channels are implemented in Phase 7.

/// Default output sample rate in Hz.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
/// M-cycles per second; the resampling accumulator's denominator.
const MCYCLES_PER_SECOND: u32 = crate::T_CYCLES_PER_SECOND / 4;
/// Upper bound on buffered samples (~1 s of stereo) if the host never drains.
const MAX_BUFFERED: usize = 2 * 48_000;

/// Bits that always read back as 1 for FF10..=FF2F (Pan Docs: "Audio Registers").
const READ_MASK: [u8; 0x20] = [
    0x80, 0x3F, 0x00, 0xFF, 0xBF, 0xFF, 0x3F, 0x00, 0xFF, 0xBF, 0x7F, 0xFF, 0x9F, 0xFF, 0xBF, 0xFF,
    0xFF, 0x00, 0x00, 0xBF, 0x00, 0x00, 0x70, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
];

#[derive(Clone, Debug)]
pub struct Apu {
    regs: [u8; 0x20],
    wave_ram: [u8; 0x10],
    sample_rate: u32,
    sample_acc: u32,
    samples: Vec<f32>,
}

impl Default for Apu {
    fn default() -> Self {
        Self::new()
    }
}

impl Apu {
    pub fn new() -> Self {
        let mut regs = [0u8; 0x20];
        // Post-boot values (Pan Docs: "Power Up Sequence").
        for (addr, v) in [
            (0xFF10, 0x80),
            (0xFF11, 0xBF),
            (0xFF12, 0xF3),
            (0xFF14, 0xBF),
            (0xFF16, 0x3F),
            (0xFF19, 0xBF),
            (0xFF1A, 0x7F),
            (0xFF1B, 0xFF),
            (0xFF1C, 0x9F),
            (0xFF1E, 0xBF),
            (0xFF20, 0xFF),
            (0xFF23, 0xBF),
            (0xFF24, 0x77),
            (0xFF25, 0xF3),
            (0xFF26, 0xF1),
        ] {
            regs[addr - 0xFF10] = v;
        }
        Self {
            regs,
            wave_ram: [0; 0x10],
            sample_rate: DEFAULT_SAMPLE_RATE,
            sample_acc: 0,
            samples: Vec::new(),
        }
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        self.sample_rate = rate.clamp(8_000, 192_000);
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF10..=0xFF2F => {
                let i = (addr - 0xFF10) as usize;
                self.regs[i] | READ_MASK[i]
            }
            0xFF30..=0xFF3F => self.wave_ram[(addr - 0xFF30) as usize],
            _ => 0xFF,
        }
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0xFF10..=0xFF2F => self.regs[(addr - 0xFF10) as usize] = value,
            0xFF30..=0xFF3F => self.wave_ram[(addr - 0xFF30) as usize] = value,
            _ => {}
        }
    }

    /// Advance one M-cycle. `div_counter` is the timer's 16-bit system counter
    /// (the frame sequencer is clocked by its bit 12 falling edge).
    pub fn tick_mcycle(&mut self, _div_counter: u16) {
        self.sample_acc += self.sample_rate;
        if self.sample_acc >= MCYCLES_PER_SECOND {
            self.sample_acc -= MCYCLES_PER_SECOND;
            if self.samples.len() < MAX_BUFFERED {
                self.samples.extend_from_slice(&[0.0, 0.0]);
            }
        }
    }

    /// Move all buffered interleaved stereo samples (L, R, L, R, ...) into `out`.
    pub fn drain_samples(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.samples);
    }
}
