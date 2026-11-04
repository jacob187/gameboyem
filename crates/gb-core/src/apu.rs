//! Audio processing unit (Pan Docs: "Audio", "Audio Registers", "Audio Details").
//!
//! Channels live in submodules; this module owns the register file, the
//! frame sequencer, NR50-NR52 handling and the mixer/resampler.
//!
//! Mixing model (decision): each channel's digital output (0..=15) is mapped
//! unipolar to 0..1 and summed per NR51 panning, scaled by the NR50 master
//! volume. The hardware DAC is bipolar with a high-pass filter; that is not
//! modelled so that a silent APU produces exactly `0.0` samples. Output is in
//! `0.0..=1.0`. Samples are the box average of the per-M-cycle mix over each
//! output period (integer accumulation, one float division per sample).

mod common;
mod square;

use square::Square;

/// Default output sample rate in Hz.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
/// M-cycles per second; the resampling accumulator's denominator.
const MCYCLES_PER_SECOND: u32 = crate::T_CYCLES_PER_SECOND / 4;
/// Upper bound on buffered samples (~1 s of stereo) if the host never drains.
const MAX_BUFFERED: usize = 2 * 48_000;
/// Bit of the 16-bit system counter that is DIV bit 4 (Pan Docs: "Audio Details").
const SEQUENCER_DIV_BIT: u16 = 1 << 12;
/// Largest per-M-cycle mix: 4 channels x 15 x master volume 8.
const MIX_MAX: f32 = 4.0 * 15.0 * 8.0;

/// Bits that always read back as 1 for FF10..=FF2F (Pan Docs: "Audio Registers").
const READ_MASK: [u8; 0x20] = [
    0x80, 0x3F, 0x00, 0xFF, 0xBF, 0xFF, 0x3F, 0x00, 0xFF, 0xBF, 0x7F, 0xFF, 0x9F, 0xFF, 0xBF, 0xFF,
    0xFF, 0x00, 0x00, 0xBF, 0x00, 0x00, 0x70, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
];

#[derive(Clone, Debug)]
pub struct Apu {
    /// Raw NR10..=NR51 as written (index = addr - 0xFF10); NR52 is derived.
    regs: [u8; 0x20],
    wave_ram: [u8; 0x10],
    ch1: Square,
    ch2: Square,
    powered: bool,
    /// Frame-sequencer step that will run on the next DIV-APU tick.
    seq_step: u8,
    last_div_bit: bool,
    sample_rate: u32,
    sample_acc: u32,
    mix_l: u32,
    mix_r: u32,
    mix_n: u32,
    samples: Vec<f32>,
}

impl Default for Apu {
    fn default() -> Self {
        Self::new()
    }
}

impl Apu {
    pub fn new() -> Self {
        let mut apu = Self {
            regs: [0; 0x20],
            wave_ram: [0; 0x10],
            ch1: Square::new(true),
            ch2: Square::new(false),
            powered: true,
            seq_step: 0,
            last_div_bit: false,
            sample_rate: DEFAULT_SAMPLE_RATE,
            sample_acc: 0,
            mix_l: 0,
            mix_r: 0,
            mix_n: 0,
            samples: Vec::new(),
        };
        // Post-boot values (Pan Docs: "Power Up Sequence"). The NRx4 values are
        // given without the trigger bit (reads are unaffected: bit 7 is masked
        // to 1) so no channel starts; CH1 is then marked active (NR52 = $F1)
        // with its envelope already decayed to silence by the boot sequence.
        for (addr, v) in [
            (0xFF10, 0x80),
            (0xFF11, 0xBF),
            (0xFF12, 0xF3),
            (0xFF13, 0xFF),
            (0xFF14, 0x3F),
            (0xFF16, 0x3F),
            (0xFF19, 0x3F),
            (0xFF1A, 0x7F),
            (0xFF1B, 0xFF),
            (0xFF1C, 0x9F),
            (0xFF1E, 0x3F),
            (0xFF20, 0xFF),
            (0xFF23, 0x3F),
            (0xFF24, 0x77),
            (0xFF25, 0xF3),
        ] {
            apu.write_reg(addr - 0xFF10, v);
        }
        apu.ch1.enabled = true;
        apu.ch1.env.volume = 0;
        apu
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        self.sample_rate = rate.clamp(8_000, 192_000);
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn status_bits(&self) -> u8 {
        u8::from(self.ch1.enabled) | u8::from(self.ch2.enabled) << 1
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF26 => 0x70 | u8::from(self.powered) << 7 | self.status_bits(),
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
            0xFF26 => self.set_power(value & 0x80 != 0),
            0xFF10..=0xFF25 => {
                if self.powered {
                    self.write_reg((addr - 0xFF10) as usize, value);
                } else {
                    // On DMG the length counters stay writable while off
                    // (Pan Docs: NR52); duty bits are not.
                    match addr {
                        0xFF11 => self.ch1.length.load(value & 0x3F),
                        0xFF16 => self.ch2.length.load(value & 0x3F),
                        _ => {}
                    }
                }
            }
            0xFF30..=0xFF3F => self.wave_ram[(addr - 0xFF30) as usize] = value,
            _ => {}
        }
    }

    /// Register write with the APU powered. `i` = addr - 0xFF10.
    fn write_reg(&mut self, i: usize, v: u8) {
        self.regs[i] = v;
        let odd = self.seq_step & 1 == 1;
        match i {
            0x00 => self.ch1.write_nr10(v),
            0x01 => self.ch1.write_duty_length(v),
            0x02 => self.ch1.write_envelope(v),
            0x03 => self.ch1.write_freq_lo(v),
            0x04 => self.ch1.write_freq_hi_ctrl(v, odd),
            0x06 => self.ch2.write_duty_length(v),
            0x07 => self.ch2.write_envelope(v),
            0x08 => self.ch2.write_freq_lo(v),
            0x09 => self.ch2.write_freq_hi_ctrl(v, odd),
            _ => {}
        }
    }

    /// NR52 bit 7. Powering off clears NR10..=NR51 and silences every channel;
    /// wave RAM and (on DMG) length counters are kept. Powering on restarts the
    /// frame sequencer at step 0 (Pan Docs: "Audio Registers", NR52).
    fn set_power(&mut self, on: bool) {
        if on == self.powered {
            return;
        }
        self.powered = on;
        if on {
            self.seq_step = 0;
            self.ch1.power_on();
            self.ch2.power_on();
        } else {
            self.regs = [0; 0x20];
            self.ch1.power_off();
            self.ch2.power_off();
        }
    }

    fn clock_sequencer(&mut self) {
        let step = self.seq_step;
        self.seq_step = (step + 1) & 7;
        if step & 1 == 0 {
            self.ch1.clock_length();
            self.ch2.clock_length();
        }
        if step == 2 || step == 6 {
            self.ch1.clock_sweep();
        }
        if step == 7 {
            self.ch1.clock_envelope();
            self.ch2.clock_envelope();
        }
    }

    /// Advance one M-cycle. `div_counter` is the timer's 16-bit system counter
    /// (the frame sequencer is clocked by its bit 12 falling edge).
    pub fn tick_mcycle(&mut self, div_counter: u16) {
        // The edge is tracked while powered off too, so powering on never sees
        // a spurious step. DIV writes / STOP clear the counter, which produces
        // the extra falling edge real hardware shows.
        let bit = div_counter & SEQUENCER_DIV_BIT != 0;
        if self.powered && self.last_div_bit && !bit {
            self.clock_sequencer();
        }
        self.last_div_bit = bit;

        if self.powered {
            self.ch1.tick_mcycle();
            self.ch2.tick_mcycle();
            self.accumulate_mix();
        }

        self.sample_acc += self.sample_rate;
        if self.sample_acc >= MCYCLES_PER_SECOND {
            self.sample_acc -= MCYCLES_PER_SECOND;
            self.emit_sample();
        }
    }

    fn accumulate_mix(&mut self) {
        let outs = [self.ch1.output(), self.ch2.output(), 0, 0];
        let nr51 = self.regs[0x15];
        let nr50 = self.regs[0x14];
        let (mut l, mut r) = (0u32, 0u32);
        for (i, &o) in outs.iter().enumerate() {
            if nr51 & (0x10 << i) != 0 {
                l += u32::from(o);
            }
            if nr51 & (1 << i) != 0 {
                r += u32::from(o);
            }
        }
        self.mix_l += l * u32::from(((nr50 >> 4) & 7) + 1);
        self.mix_r += r * u32::from((nr50 & 7) + 1);
        self.mix_n += 1;
    }

    fn emit_sample(&mut self) {
        let (l, r) = if self.mix_n == 0 {
            (0.0, 0.0)
        } else {
            let d = self.mix_n as f32 * MIX_MAX;
            (self.mix_l as f32 / d, self.mix_r as f32 / d)
        };
        self.mix_l = 0;
        self.mix_r = 0;
        self.mix_n = 0;
        if self.samples.len() < MAX_BUFFERED {
            self.samples.extend_from_slice(&[l, r]);
        }
    }

    /// Move all buffered interleaved stereo samples (L, R, L, R, ...) into `out`.
    pub fn drain_samples(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.samples);
    }
}

#[cfg(test)]
mod tests;
