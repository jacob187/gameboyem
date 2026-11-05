//! Sound: four PSG channels, Direct Sound FIFO A/B, and the SOUNDCNT/SOUNDBIAS
//! mixer (GBATEK "GBA Sound Controller"). Built in phase P2 (P2-APU).
//! **Contract** for the bus: FIFO writes (0x0400_00A0 / 0x0400_00A4) and
//! [`Apu::on_timer_overflow`].
//!
//! # Timing and resampling (DECISIONS #60)
//!
//! The APU is lazily synchronised to the master clock: [`Apu::advance_to`]
//! runs it up to a timestamp, and the bus calls it before every sound register
//! write, every timer overflow that clocks a FIFO, before the audio is drained
//! and every [`PUMP_CYCLES`] cycles (the `Event::ApuSample` pump). Between two
//! such points nothing the APU hears changes except the channels' own timers,
//! which `advance_to` steps exactly: it splits the elapsed cycles at every
//! channel step, frame-sequencer step and host-sample boundary.
//!
//! The mix is integrated over each host sample (a box filter over its
//! `16.78 MHz / rate` cycles), so no per-sample rounding of the channel phase
//! and no separate internal sample rate: the host sample count is
//! `floor(rate * cycles / 2^24)` exactly, with no drift. SOUNDBIAS bits 14-15
//! (the hardware's 6..9-bit / 262..32 kHz resolution) are stored but not
//! modelled. No clocks, no randomness, no floating point in the state: the
//! same register writes at the same cycles give bit-identical audio.

mod common;
mod fifo;
mod noise;
mod square;
mod wave;

use crate::CYCLES_PER_SECOND;
use fifo::Fifo;
use noise::Noise;
use square::Square;
use wave::Wave;

/// Which FIFOs want a DMA refill after a timer overflow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FifoRequest {
    pub a: bool,
    pub b: bool,
}

/// Default host sample rate until the frontend sets its device rate.
pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;

/// Interval of the bus's `Event::ApuSample` pump in CPU cycles: one hardware
/// output sample (32.768 kHz, GBATEK SOUNDBIAS). It only bounds how far the
/// APU can lag the clock; the output is exact without it.
pub const PUMP_CYCLES: u64 = 512;

/// CPU cycles per frame-sequencer step: 512 Hz (length 256 Hz on even steps,
/// sweep 128 Hz on steps 2 and 6, envelope 64 Hz on step 7; the GB sequencer
/// the GBA PSG shares, GBATEK gives the three rates).
const SEQUENCER_CYCLES: u32 = CYCLES_PER_SECOND / 512;

/// Seconds of audio kept if the host never drains.
const MAX_BUFFERED_SECS: usize = 1;

/// Mix units: a quarter of one SOUNDBIAS output LSB, so the PSG's 25% ratio
/// stays integral. The 10-bit output (0..=0x3FF) spans `0..=4095` units.
const MIX_MAX: i32 = 0x3FF * 4 + 3;
/// The unit value of the output's centre (bias 0x200).
const MIX_CENTRE: f64 = 2048.0;

/// Register offsets (from 0x0400_0000).
const SOUNDCNT_L: u32 = 0x80;
const SOUNDCNT_H: u32 = 0x82;
const SOUNDCNT_X: u32 = 0x84;
const SOUNDBIAS: u32 = 0x88;
const WAVE_RAM: u32 = 0x90;

/// Readable bits of the sound registers 0x060..0x0A0 (GBATEK "GBA Sound
/// Controller" register tables: length/initial-frequency bits are write-only).
/// SOUNDCNT_X's entry is composed from live state.
const READ_MASK: [u16; 0x20] = [
    0x007F, 0xFFC0, 0x4000, 0, 0xFFC0, 0, 0x4000, 0, // 0x60..0x70: PSG 1, 2
    0x00E0, 0xE000, 0x4000, 0, 0xFF00, 0, 0x40FF, 0, // 0x70..0x80: PSG 3, 4
    0xFF77, 0x770F, 0x008F, 0, 0xC3FE, 0, 0, 0, // 0x80..0x90: SOUNDCNT, BIAS
    0, 0, 0, 0, 0, 0, 0, 0, // wave RAM is read from the banks
];

/// SOUNDBIAS stores bits 1-9 (level) and 14-15; the added bias is bits 1-9
/// as a 10-bit value (default 0x200).
const SOUNDBIAS_MASK: u16 = 0xC3FE;
const SOUNDBIAS_DEFAULT: u16 = 0x0200;

#[inline]
fn merge(old: u16, v: u16, mask: u16) -> u16 {
    (old & !mask) | (v & mask)
}

#[derive(Debug, Clone)]
pub struct Apu {
    sample_rate: u32,
    /// Master-clock time the APU has been run to.
    now: u64,
    /// Raw sound registers 0x060..0x0A0 (SOUNDCNT_X keeps only bit 7).
    regs: [u16; 0x20],
    /// SOUNDCNT_X bit 7.
    master: bool,
    ch1: Square,
    ch2: Square,
    ch3: Wave,
    ch4: Noise,
    fifo: [Fifo; 2],
    /// Frame-sequencer step that runs next, and cycles until it does.
    seq_step: u8,
    seq_timer: u32,
    /// The current output level per side in mix units, bias and clamp included.
    cur_l: i32,
    cur_r: i32,
    /// Sums of the level over the cycles of the host sample in progress.
    mix_l: i64,
    mix_r: i64,
    mix_n: u32,
    /// Host-sample phase: `rate` is added per cycle; a sample completes at
    /// `CYCLES_PER_SECOND`.
    sample_acc: u64,
    samples: Vec<f32>,
}

impl Default for Apu {
    fn default() -> Self {
        let mut apu = Self {
            sample_rate: DEFAULT_SAMPLE_RATE,
            now: 0,
            regs: [0; 0x20],
            master: false,
            ch1: Square::new(true),
            ch2: Square::new(false),
            ch3: Wave::new(),
            ch4: Noise::new(),
            fifo: [Fifo::default(), Fifo::default()],
            seq_step: 0,
            seq_timer: SEQUENCER_CYCLES,
            cur_l: 0,
            cur_r: 0,
            mix_l: 0,
            mix_r: 0,
            mix_n: 0,
            sample_acc: 0,
            samples: Vec::new(),
        };
        apu.regs[Self::index(SOUNDBIAS)] = SOUNDBIAS_DEFAULT;
        apu.update_mix();
        apu
    }
}

impl Apu {
    fn index(off: u32) -> usize {
        (off as usize - 0x60) >> 1
    }

    /// Timer `n` (0 or 1) overflowed: FIFOs clocked by it pop a sample, and any
    /// that fall to 16 bytes or fewer request a refill. **Contract.** The bus
    /// calls [`Apu::advance_to`] with the overflow's time first.
    pub fn on_timer_overflow(&mut self, n: usize) -> FifoRequest {
        let cnt_h = self.regs[Self::index(SOUNDCNT_H)];
        let mut req = FifoRequest::default();
        let mut changed = false;
        for b in 0..2 {
            // SOUNDCNT_H bit 10 (A) / 14 (B): timer select.
            if usize::from(cnt_h >> (10 + 4 * b) & 1) == n {
                changed |= self.fifo[b].pop();
                let wants = self.fifo[b].wants_refill();
                if b == 0 {
                    req.a = wants;
                } else {
                    req.b = wants;
                }
            }
        }
        if changed {
            self.update_mix();
        }
        req
    }

    /// A 32-bit write to FIFO A (`b = false`) or B: four signed 8-bit samples,
    /// lowest byte first. **Contract.**
    pub fn write_fifo(&mut self, b: bool, value: u32) {
        self.fifo[usize::from(b)].push_word(value);
    }

    /// Append the stereo samples produced since the last call, interleaved
    /// `[l, r, l, r, ...]` in -1.0..=1.0 at [`Apu::sample_rate`], the same
    /// format as `gb-core`. **Contract.** The stream covers the time the APU
    /// has been run to; [`crate::Gba::drain_audio_into`] syncs it to the
    /// clock first. 0.0 is the SOUNDBIAS centre (0x200): silence with the
    /// default bias.
    pub fn drain_samples(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.samples);
    }

    /// Host output rate in Hz. **Contract.**
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Set the host output rate (clamped to 8 kHz..=192 kHz). **Contract.**
    pub fn set_sample_rate(&mut self, hz: u32) {
        self.sample_rate = hz.clamp(8_000, 192_000);
    }

    /// The master-clock time the APU has been run to.
    pub fn now(&self) -> u64 {
        self.now
    }

    /// Upper bound on buffered interleaved stereo samples.
    fn max_buffered(&self) -> usize {
        2 * self.sample_rate as usize * MAX_BUFFERED_SECS
    }

    /// Run the APU up to master-clock time `t` (a no-op if it is already there).
    pub fn advance_to(&mut self, t: u64) {
        if t > self.now {
            let cycles = t - self.now;
            self.now = t;
            self.run(cycles);
        }
    }

    fn run(&mut self, mut remaining: u64) {
        let cps = u64::from(CYCLES_PER_SECOND);
        let rate = u64::from(self.sample_rate);
        while remaining > 0 {
            // Cycles until the host sample in progress completes (>= 1).
            let to_sample = (cps - self.sample_acc).div_ceil(rate);
            let mut seg = remaining.min(to_sample);
            if self.master {
                seg = seg.min(u64::from(self.seq_timer));
                let next = [
                    self.ch1.next_step(),
                    self.ch2.next_step(),
                    self.ch3.next_step(),
                    self.ch4.next_step(),
                ];
                for t in next.into_iter().flatten() {
                    seg = seg.min(u64::from(t));
                }
            }
            let s = seg as u32;
            self.mix_l += i64::from(self.cur_l) * i64::from(s);
            self.mix_r += i64::from(self.cur_r) * i64::from(s);
            self.mix_n += s;
            self.sample_acc += rate * seg;
            remaining -= seg;
            if self.master {
                self.step_channels(s);
            }
            if self.sample_acc >= cps {
                self.sample_acc -= cps;
                self.emit_sample();
            }
        }
    }

    fn step_channels(&mut self, s: u32) {
        let mut dirty = false;
        self.seq_timer -= s;
        if self.seq_timer == 0 {
            self.seq_timer = SEQUENCER_CYCLES;
            self.clock_sequencer();
            dirty = true;
        }
        // Non-short-circuiting `|`: every channel must be advanced.
        dirty |=
            self.ch1.advance(s) | self.ch2.advance(s) | self.ch3.advance(s) | self.ch4.advance(s);
        if dirty {
            self.update_mix();
        }
    }

    fn clock_sequencer(&mut self) {
        let step = self.seq_step;
        self.seq_step = (step + 1) & 7;
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

    /// Recompute the per-side level in mix units (GBATEK "SOUNDCNT_H", "SOUNDBIAS"):
    /// * PSG: the enabled channels' 0..=15 outputs summed per side, times the
    ///   SOUNDCNT_L master volume + 1 (1..=8), times the SOUNDCNT_H ratio
    ///   (25% / 50% / 100% = 1 / 2 / 4 units; the prohibited value 3 is mute).
    ///   Channels are unsigned like the GB's DAC levels; their DC offset rides
    ///   on the bias.
    /// * Direct Sound: the signed 8-bit sample times 8 (50%) or 16 (100%) units
    ///   (GBATEK: a FIFO spans +-0x200, a PSG channel +-0x80, all +-0x600).
    /// * Plus the SOUNDBIAS level, clamped to the 10-bit output range.
    ///
    /// With SOUNDCNT_X bit 7 clear the PSG and FIFOs contribute nothing.
    fn update_mix(&mut self) {
        let (mut l, mut r) = (0i32, 0i32);
        if self.master {
            let cnt_l = self.regs[Self::index(SOUNDCNT_L)];
            let cnt_h = self.regs[Self::index(SOUNDCNT_H)];
            let outs = [
                self.ch1.output(),
                self.ch2.output(),
                self.ch3.output(),
                self.ch4.output(),
            ];
            let (mut sum_l, mut sum_r) = (0i32, 0i32);
            for (i, &o) in outs.iter().enumerate() {
                if cnt_l >> (8 + i) & 1 != 0 {
                    sum_r += i32::from(o);
                }
                if cnt_l >> (12 + i) & 1 != 0 {
                    sum_l += i32::from(o);
                }
            }
            let ratio = match cnt_h & 3 {
                0 => 1,
                1 => 2,
                2 => 4,
                _ => 0,
            };
            l = sum_l * i32::from((cnt_l >> 4 & 7) + 1) * ratio;
            r = sum_r * i32::from((cnt_l & 7) + 1) * ratio;
            for b in 0..2 {
                // A: bits 8 (R), 9 (L), 2 (volume); B: bits 12, 13, 3.
                let sh = 4 * b;
                let scale = if cnt_h >> (2 + b) & 1 != 0 { 16 } else { 8 };
                let s = i32::from(self.fifo[b].sample) * scale;
                if cnt_h >> (8 + sh) & 1 != 0 {
                    r += s;
                }
                if cnt_h >> (9 + sh) & 1 != 0 {
                    l += s;
                }
            }
        }
        let bias = i32::from(self.regs[Self::index(SOUNDBIAS)] & 0x03FE) * 4;
        self.cur_l = (l + bias).clamp(0, MIX_MAX);
        self.cur_r = (r + bias).clamp(0, MIX_MAX);
    }

    fn emit_sample(&mut self) {
        let n = f64::from(self.mix_n);
        let level = |sum: i64| ((sum as f64 / n / MIX_CENTRE - 1.0) as f32).clamp(-1.0, 1.0);
        let (l, r) = (level(self.mix_l), level(self.mix_r));
        self.mix_l = 0;
        self.mix_r = 0;
        self.mix_n = 0;
        if self.samples.len() < self.max_buffered() {
            self.samples.extend_from_slice(&[l, r]);
        }
    }

    /// Read the sound register at the even I/O offset `off` (0x060..=0x09F).
    /// Only the readable bits show (the old `SOUND_READ_MASK`).
    pub fn read_reg(&self, off: u32) -> u16 {
        match off {
            SOUNDCNT_X => {
                // Bits 0-3: channel ON flags; bit 7: master enable.
                u16::from(self.master) << 7
                    | u16::from(self.ch1.enabled)
                    | u16::from(self.ch2.enabled) << 1
                    | u16::from(self.ch3.enabled) << 2
                    | u16::from(self.ch4.enabled) << 3
            }
            WAVE_RAM..=0x9F => {
                let bank = &self.ch3.ram[self.ch3.cpu_bank()];
                let i = (off - WAVE_RAM) as usize;
                u16::from_le_bytes([bank[i], bank[i + 1]])
            }
            0x60..=0x8F => {
                let i = Self::index(off);
                self.regs[i] & READ_MASK[i]
            }
            _ => 0,
        }
    }

    /// Write the sound register at the even I/O offset `off`; `mask` selects the
    /// byte lanes written. The bus calls [`Apu::advance_to`] first.
    pub fn write_reg(&mut self, off: u32, v: u16, mask: u16) {
        match off {
            SOUNDCNT_X => {
                if mask & 0xFF != 0 {
                    self.set_master(v & 0x80 != 0);
                }
            }
            SOUNDCNT_H => {
                // Stays writable while the sound is off, but does nothing then
                // (GBATEK SOUNDCNT_X). Bits 11 and 15 reset a FIFO and don't stick.
                let i = Self::index(off);
                self.regs[i] = merge(self.regs[i], v, mask & 0x770F);
                for b in 0..2 {
                    if v & mask & (0x0800 << (4 * b)) != 0 {
                        self.fifo[b].reset();
                    }
                }
                self.update_mix();
            }
            SOUNDBIAS => {
                let i = Self::index(off);
                self.regs[i] = merge(self.regs[i], v, mask & SOUNDBIAS_MASK);
                self.update_mix();
            }
            WAVE_RAM..=0x9F => {
                let bank = self.ch3.cpu_bank();
                let i = (off - WAVE_RAM) as usize;
                let cur = u16::from_le_bytes([self.ch3.ram[bank][i], self.ch3.ram[bank][i + 1]]);
                let new = merge(cur, v, mask).to_le_bytes();
                self.ch3.ram[bank][i] = new[0];
                self.ch3.ram[bank][i + 1] = new[1];
            }
            0x60..=SOUNDCNT_L => {
                // While SOUNDCNT_X bit 7 is clear these registers are held at
                // zero: writes are ignored (GBATEK SOUNDCNT_X).
                if !self.master {
                    return;
                }
                let i = Self::index(off);
                let new = merge(self.regs[i], v, mask);
                self.regs[i] = new;
                if mask & 0x00FF != 0 {
                    self.write_psg_byte(off, new as u8);
                }
                if mask & 0xFF00 != 0 {
                    self.write_psg_byte(off + 1, (new >> 8) as u8);
                }
                self.update_mix();
            }
            _ => {}
        }
    }

    /// One byte lane of a PSG register: the GB-style NRxx layout the halfword
    /// registers are made of. A restart (bit 7 of a frequency high byte) acts
    /// only when that lane is written.
    fn write_psg_byte(&mut self, addr: u32, b: u8) {
        // The sequencer step that runs next is odd: the next step does not
        // clock length (see `common::Length`).
        let odd = self.seq_step & 1 == 1;
        match addr {
            0x60 => self.ch1.write_nr10(b),
            0x62 => self.ch1.write_duty_length(b),
            0x63 => self.ch1.write_envelope(b),
            0x64 => self.ch1.write_freq_lo(b),
            0x65 => self.ch1.write_freq_hi_ctrl(b, odd),
            0x68 => self.ch2.write_duty_length(b),
            0x69 => self.ch2.write_envelope(b),
            0x6C => self.ch2.write_freq_lo(b),
            0x6D => self.ch2.write_freq_hi_ctrl(b, odd),
            0x70 => self.ch3.write_nr30(b),
            0x72 => self.ch3.write_length(b),
            0x73 => self.ch3.write_volume(b),
            0x74 => self.ch3.write_freq_lo(b),
            0x75 => self.ch3.write_freq_hi_ctrl(b, odd),
            0x78 => self.ch4.write_length(b),
            0x79 => self.ch4.write_envelope(b),
            0x7C => self.ch4.write_nr43(b),
            0x7D => self.ch4.write_ctrl(b, odd),
            _ => {}
        }
    }

    /// SOUNDCNT_X bit 7. Clearing it resets every PSG register and channel (wave
    /// RAM stays); setting it restarts the frame sequencer (GBATEK SOUNDCNT_X).
    fn set_master(&mut self, on: bool) {
        if on == self.master {
            return;
        }
        self.master = on;
        self.seq_step = 0;
        self.seq_timer = SEQUENCER_CYCLES;
        if !on {
            self.regs[..=Self::index(SOUNDCNT_L)].fill(0);
            self.ch1.reset();
            self.ch2.reset();
            self.ch3.reset();
            self.ch4.reset();
        }
        self.update_mix();
    }
}

#[cfg(test)]
mod tests;
