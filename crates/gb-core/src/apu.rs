//! Audio processing unit (Pan Docs: "Audio", "Audio Registers", "Audio Details").
//!
//! Channels live in submodules; this module owns the register file, the
//! frame sequencer, NR50-NR52 handling and the mixer/resampler.
//!
//! Mixing model: each channel's digital output (0..=15) goes through a
//! bipolar DAC (digital 0 = analog +1, 15 = -1, a powered-down DAC = 0),
//! channels are summed per NR51 panning and scaled by the NR50 master volume,
//! then each side passes through the DMG high-pass filter (capacitor model,
//! see [`HpfCap`]). Samples are the box average of the per-M-cycle mix over
//! each output period (integer accumulation, one float division per sample),
//! normalised so a full-scale mix is +-1: the stream is signed, in
//! `-1.0..=1.0`. A powered-off APU, or one whose four DACs are all off,
//! produces exactly `0.0`.

mod common;
mod noise;
mod square;
mod wave;

use crate::state::{StateError, StateReader, StateWriter, check};
use noise::Noise;
use square::Square;
use wave::Wave;

/// Default output sample rate in Hz.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
/// M-cycles per second; the resampling accumulator's denominator.
const MCYCLES_PER_SECOND: u32 = crate::T_CYCLES_PER_SECOND / 4;
/// Seconds of audio kept if the host never drains (the cap scales with rate).
const MAX_BUFFERED_SECS: usize = 1;
/// Bit of the 16-bit system counter that is DIV bit 4 (Pan Docs: "Audio Details").
const SEQUENCER_DIV_BIT: u16 = 1 << 12;
/// Largest per-M-cycle mix magnitude: 4 channels x 15 x master volume 8. A
/// channel contributes `15 - 2 * digital` (the DAC's +-15 in 1/15 units).
const MIX_MAX: f32 = 4.0 * 15.0 * 8.0;
/// DMG high-pass charge factor per T-cycle (Pan Docs: "Audio Details").
const HPF_CHARGE: f64 = 0.999_958;
/// `1.0 - HPF_CHARGE`, written out so it is exact in decimal.
const HPF_LOSS: f64 = 0.000_042;

/// Bits that always read back as 1 for FF10..=FF2F (Pan Docs: "Audio Registers").
const READ_MASK: [u8; 0x20] = [
    0x80, 0x3F, 0x00, 0xFF, 0xBF, 0xFF, 0x3F, 0x00, 0xFF, 0xBF, 0x7F, 0xFF, 0x9F, 0xFF, 0xBF, 0xFF,
    0xFF, 0x00, 0x00, 0xBF, 0x00, 0x00, 0x70, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
];

/// Per-sample DMG charge factor: `0.999958 ^ (4194304 / rate)`.
///
/// Computed with plain IEEE f64 multiplications (square-and-multiply for the
/// integer part, a binomial series for the fraction, error < 1e-13) rather
/// than `powf`, whose last bits differ between libm implementations.
fn hpf_factor(rate: u32) -> f32 {
    let exp = f64::from(crate::T_CYCLES_PER_SECOND) / f64::from(rate);
    let whole = exp.floor();
    let frac = exp - whole;
    let (mut acc, mut base, mut n) = (1.0f64, HPF_CHARGE, whole as u32);
    while n > 0 {
        if n & 1 == 1 {
            acc *= base;
        }
        base *= base;
        n >>= 1;
    }
    acc *= 1.0 - frac * HPF_LOSS + frac * (frac - 1.0) * 0.5 * HPF_LOSS * HPF_LOSS;
    acc as f32
}

/// High-pass filter capacitor for one output side (Pan Docs: "Audio Details").
/// `out = in - cap`, then the capacitor charges towards `in` through the
/// output: `cap = in - out * factor`. Both are IEEE f32 operations.
#[derive(Clone, Copy, Debug, Default)]
struct HpfCap(f32);

impl HpfCap {
    /// Filter one sample; returns the unclamped output.
    fn step(&mut self, input: f32, factor: f32) -> f32 {
        let out = input - self.0;
        self.0 = input - out * factor;
        out
    }
}

#[derive(Clone, Debug)]
pub struct Apu {
    /// Raw NR10..=NR51 as written (index = addr - 0xFF10); NR52 is derived.
    regs: [u8; 0x20],
    ch1: Square,
    ch2: Square,
    ch3: Wave,
    ch4: Noise,
    powered: bool,
    /// Frame-sequencer step that will run on the next DIV-APU tick.
    seq_step: u8,
    last_div_bit: bool,
    /// Powered on while DIV bit 4 was high: the next DIV-APU event is lost.
    skip_div_event: bool,
    sample_rate: u32,
    sample_acc: u32,
    /// Weighted mix of the current M-cycle; recomputed only when `dirty`.
    cur_l: i32,
    cur_r: i32,
    /// At least one channel DAC is powered (the HPF capacitor is connected).
    dac_any: bool,
    dirty: bool,
    mix_l: i32,
    mix_r: i32,
    mix_n: u32,
    hpf_factor: f32,
    cap_l: HpfCap,
    cap_r: HpfCap,
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
            ch1: Square::new(true),
            ch2: Square::new(false),
            ch3: Wave::new(),
            ch4: Noise::new(),
            powered: true,
            seq_step: 0,
            last_div_bit: false,
            skip_div_event: false,
            sample_rate: DEFAULT_SAMPLE_RATE,
            sample_acc: 0,
            cur_l: 0,
            cur_r: 0,
            dac_any: false,
            dirty: true,
            mix_l: 0,
            mix_r: 0,
            mix_n: 0,
            hpf_factor: hpf_factor(DEFAULT_SAMPLE_RATE),
            cap_l: HpfCap::default(),
            cap_r: HpfCap::default(),
            samples: Vec::new(),
        };
        // Post-boot values (Pan Docs: "Power Up Sequence"). The NRx4 values are
        // given without the trigger bit (reads are unaffected: bit 7 is masked
        // to 1) so no channel starts; CH1 is then marked active (NR52 = $F1)
        // with its envelope already decayed to silence by the boot sequence.
        // NR13/NR23/NR33 are write-only (they read back $FF whatever is
        // stored) but the table lists $FF for all three, so the internal
        // period low byte of CH1-CH3 starts at $FF (Pan Docs: "Power Up Sequence").
        for (addr, v) in [
            (0xFF10, 0x80),
            (0xFF11, 0xBF),
            (0xFF12, 0xF3),
            (0xFF13, 0xFF),
            (0xFF14, 0x3F),
            (0xFF16, 0x3F),
            (0xFF18, 0xFF),
            (0xFF19, 0x3F),
            (0xFF1A, 0x7F),
            (0xFF1B, 0xFF),
            (0xFF1C, 0x9F),
            (0xFF1D, 0xFF),
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
        // The boot ROM ran long enough for the capacitors to settle on the
        // resulting DC level, so the first samples are silent, not a click.
        apu.update_mix();
        apu.dirty = false;
        apu.cap_l = HpfCap(Self::level(apu.cur_l, 1));
        apu.cap_r = HpfCap(Self::level(apu.cur_r, 1));
        apu
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        self.sample_rate = rate.clamp(8_000, 192_000);
        self.hpf_factor = hpf_factor(self.sample_rate);
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Upper bound on buffered interleaved stereo samples.
    fn max_buffered(&self) -> usize {
        2 * self.sample_rate as usize * MAX_BUFFERED_SECS
    }

    fn status_bits(&self) -> u8 {
        u8::from(self.ch1.enabled)
            | u8::from(self.ch2.enabled) << 1
            | u8::from(self.ch3.enabled) << 2
            | u8::from(self.ch4.enabled) << 3
    }

    /// CGB PCM12 (0xFF76) / PCM34 (0xFF77): the channels' current digital
    /// outputs, CH1 / CH3 in the low nibble and CH2 / CH4 in the high one. Both
    /// read 0 while the APU is powered off (NR52.7 = 0), and a channel whose DAC
    /// is off (NRx2 bits 7-3 clear, or NR30.7 clear) outputs 0. A channel with
    /// its DAC on but disabled (length expiry) also outputs 0, via `output`.
    pub(crate) fn read_pcm(&self, addr: u16) -> u8 {
        if !self.powered {
            return 0;
        }
        let nibble = |dac: bool, out: u8| if dac { out } else { 0 };
        let (lo, hi) = if addr == 0xFF76 {
            (
                nibble(self.ch1.dac_on(), self.ch1.output()),
                nibble(self.ch2.dac_on(), self.ch2.output()),
            )
        } else {
            (
                nibble(self.ch3.dac_on(), self.ch3.output()),
                nibble(self.ch4.dac_on(), self.ch4.output()),
            )
        };
        lo | hi << 4
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF26 => 0x70 | u8::from(self.powered) << 7 | self.status_bits(),
            0xFF10..=0xFF2F => {
                let i = (addr - 0xFF10) as usize;
                self.regs[i] | READ_MASK[i]
            }
            0xFF30..=0xFF3F => self
                .ch3
                .ram_index((addr - 0xFF30) as usize)
                .map_or(0xFF, |i| self.ch3.ram[i]),
            _ => 0xFF,
        }
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        self.dirty = true;
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
                        0xFF1B => self.ch3.write_length(value),
                        0xFF20 => self.ch4.write_length(value),
                        _ => {}
                    }
                }
            }
            0xFF30..=0xFF3F => {
                if let Some(i) = self.ch3.ram_index((addr - 0xFF30) as usize) {
                    self.ch3.ram[i] = value;
                }
            }
            _ => {}
        }
    }

    /// Register write with the APU powered. `i` = addr - 0xFF10.
    fn write_reg(&mut self, i: usize, v: u8) {
        self.regs[i] = v;
        let odd = self.seq_step & 1 == 1;
        let env_next = self.seq_step == 7;
        match i {
            0x00 => self.ch1.write_nr10(v),
            0x01 => self.ch1.write_duty_length(v),
            0x02 => self.ch1.write_envelope(v),
            0x03 => self.ch1.write_freq_lo(v),
            0x04 => self.ch1.write_freq_hi_ctrl(v, odd, env_next),
            0x06 => self.ch2.write_duty_length(v),
            0x07 => self.ch2.write_envelope(v),
            0x08 => self.ch2.write_freq_lo(v),
            0x09 => self.ch2.write_freq_hi_ctrl(v, odd, env_next),
            0x0A => self.ch3.write_nr30(v),
            0x0B => self.ch3.write_length(v),
            0x0C => self.ch3.write_level(v),
            0x0D => self.ch3.write_freq_lo(v),
            0x0E => self.ch3.write_freq_hi_ctrl(v, odd),
            0x10 => self.ch4.write_length(v),
            0x11 => self.ch4.write_envelope(v),
            0x12 => self.ch4.write_nr43(v),
            0x13 => self.ch4.write_ctrl(v, odd, env_next),
            _ => {}
        }
    }

    /// NR52 bit 7. Powering off clears NR10..=NR51 and silences every channel;
    /// wave RAM and (on DMG) length counters are kept. Powering on restarts the
    /// frame sequencer at step 0 (Pan Docs: "Audio Registers", NR52). If DIV
    /// bit 4 is high at that moment, the first DIV-APU event is skipped (the
    /// sequencer waits for the following falling edge to run step 0).
    fn set_power(&mut self, on: bool) {
        if on == self.powered {
            return;
        }
        self.powered = on;
        self.dirty = true;
        self.skip_div_event = false;
        if on {
            self.seq_step = 0;
            self.skip_div_event = self.last_div_bit;
            self.ch1.power_on();
            self.ch2.power_on();
            self.ch3.power_on();
        } else {
            self.regs = [0; 0x20];
            // Every DAC is off now: the output is cut and the capacitors
            // drain, so a later power-on starts from a clean slate.
            self.dac_any = false;
            self.cur_l = 0;
            self.cur_r = 0;
            self.cap_l = HpfCap::default();
            self.cap_r = HpfCap::default();
            self.ch1.power_off();
            self.ch2.power_off();
            self.ch3.power_off();
            self.ch4.power_off();
        }
    }

    fn clock_sequencer(&mut self) {
        if std::mem::take(&mut self.skip_div_event) {
            return;
        }
        let step = self.seq_step;
        self.seq_step = (step + 1) & 7;
        self.dirty = true;
        if step & 1 == 0 {
            self.ch1.clock_length();
            self.ch2.clock_length();
            self.ch3.clock_length();
            self.ch4.clock_length();
        }
        if step == 2 || step == 6 {
            self.ch1.clock_sweep();
        }
        if step == 7 {
            self.ch1.clock_envelope();
            self.ch2.clock_envelope();
            self.ch4.clock_envelope();
        }
    }

    /// Advance one M-cycle of real time. `div_counter` is the timer's 16-bit
    /// system counter (the frame sequencer is clocked by its bit 12 falling edge).
    /// In CPU double speed the bus calls this every other CPU M-cycle with the
    /// counter shifted right once, so the sequencer sees counter bit 13 and
    /// everything else keeps its real-time rate.
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
            // Non-short-circuiting `|`: every channel must be ticked.
            self.dirty |= self.ch1.tick_mcycle()
                | self.ch2.tick_mcycle()
                | self.ch3.tick_mcycle()
                | self.ch4.tick_mcycle();
            if self.dirty {
                self.dirty = false;
                self.update_mix();
            }
            self.mix_l += self.cur_l;
            self.mix_r += self.cur_r;
            self.mix_n += 1;
        }

        self.sample_acc += self.sample_rate;
        if self.sample_acc >= MCYCLES_PER_SECOND {
            self.sample_acc -= MCYCLES_PER_SECOND;
            self.emit_sample();
        }
    }

    /// Recompute the weighted per-side mix: per channel the bipolar DAC value
    /// `15 - 2 * digital` (0 if its DAC is off), summed per NR51 side and
    /// multiplied by the NR50 master volume 1..=8. Range `-480..=480`.
    fn update_mix(&mut self) {
        let chans = [
            (self.ch1.dac_on(), self.ch1.output()),
            (self.ch2.dac_on(), self.ch2.output()),
            (self.ch3.dac_on(), self.ch3.output()),
            (self.ch4.dac_on(), self.ch4.output()),
        ];
        let nr51 = self.regs[0x15];
        let nr50 = self.regs[0x14];
        let (mut l, mut r) = (0i32, 0i32);
        for (i, &(dac, o)) in chans.iter().enumerate() {
            let analog = if dac { 15 - 2 * i32::from(o) } else { 0 };
            if nr51 & (0x10 << i) != 0 {
                l += analog;
            }
            if nr51 & (1 << i) != 0 {
                r += analog;
            }
        }
        self.dac_any = chans.iter().any(|&(dac, _)| dac);
        self.cur_l = l * i32::from(((nr50 >> 4) & 7) + 1);
        self.cur_r = r * i32::from((nr50 & 7) + 1);
    }

    /// Normalised level of a mix sum accumulated over `n` M-cycles.
    fn level(sum: i32, n: u32) -> f32 {
        sum as f32 / (n as f32 * MIX_MAX)
    }

    fn emit_sample(&mut self) {
        let (l, r) = if self.powered && self.dac_any && self.mix_n != 0 {
            let n = self.mix_n;
            let f = self.hpf_factor;
            let l = self.cap_l.step(Self::level(self.mix_l, n), f);
            let r = self.cap_r.step(Self::level(self.mix_r, n), f);
            // The filter can overshoot full scale when a channel flips from
            // one extreme to the other, so clamp the emitted value.
            (l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0))
        } else {
            // Powered off or every DAC off: the mixer is disconnected.
            (0.0, 0.0)
        };
        self.mix_l = 0;
        self.mix_r = 0;
        self.mix_n = 0;
        if self.samples.len() < self.max_buffered() {
            self.samples.extend_from_slice(&[l, r]);
        }
    }

    /// Move all buffered interleaved stereo samples (L, R, L, R, ...) into
    /// `out`. The stream is signed: every value is in `-1.0..=1.0`, centred on
    /// 0 by the DMG high-pass filter, and exactly `0.0` while the APU is off or
    /// all DACs are off. (Before the bipolar DAC model the range was
    /// `0.0..=1.0`; hosts that assumed unipolar data must not offset it.)
    pub fn drain_samples(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.samples);
    }
}

#[cfg(test)]
mod tests;
impl Apu {
    /// The sample rate, its filter factor and buffered samples are host-side
    /// and not part of the state.
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.bytes(&self.regs);
        self.ch1.save_state(w);
        self.ch2.save_state(w);
        self.ch3.save_state(w);
        self.ch4.save_state(w);
        w.bool(self.powered);
        w.u8(self.seq_step);
        w.bool(self.last_div_bit);
        w.bool(self.skip_div_event);
        w.u32(self.sample_acc);
        w.i32(self.cur_l);
        w.i32(self.cur_r);
        w.bool(self.dac_any);
        w.bool(self.dirty);
        w.i32(self.mix_l);
        w.i32(self.mix_r);
        w.u32(self.mix_n);
        w.f32(self.cap_l.0);
        w.f32(self.cap_r.0);
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.bytes_into(&mut self.regs)?;
        self.ch1.load_state(r)?;
        self.ch2.load_state(r)?;
        self.ch3.load_state(r)?;
        self.ch4.load_state(r)?;
        self.powered = r.bool()?;
        self.seq_step = r.u8()?;
        check(self.seq_step <= 7, "frame sequencer step")?;
        self.last_div_bit = r.bool()?;
        self.skip_div_event = r.bool()?;
        self.sample_acc = r.u32()?;
        check(self.sample_acc < MCYCLES_PER_SECOND, "sample accumulator")?;
        // Per-M-cycle mix: 4 channels x level 15 x volume 8.
        const MAX_CUR: i32 = 4 * 15 * 8;
        self.cur_l = r.i32()?;
        self.cur_r = r.i32()?;
        check(
            self.cur_l.abs() <= MAX_CUR && self.cur_r.abs() <= MAX_CUR,
            "mix level",
        )?;
        self.dac_any = r.bool()?;
        self.dirty = r.bool()?;
        self.mix_l = r.i32()?;
        self.mix_r = r.i32()?;
        self.mix_n = r.u32()?;
        // One output sample spans at most MCYCLES_PER_SECOND / 8000 M-cycles.
        check(self.mix_n <= MCYCLES_PER_SECOND / 8_000 + 1, "mix count")?;
        let max_mix = i64::from(self.mix_n) * i64::from(MAX_CUR);
        check(
            i64::from(self.mix_l).abs() <= max_mix && i64::from(self.mix_r).abs() <= max_mix,
            "mix sum",
        )?;
        self.cap_l = HpfCap(r.f32()?);
        self.cap_r = HpfCap(r.f32()?);
        Ok(())
    }
}
