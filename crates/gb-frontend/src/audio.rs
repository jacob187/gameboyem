//! Audio output: a bounded ring buffer fed by the emulation thread and drained
//! by the cpal callback. Any failure degrades to "no audio".

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, TryLockError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};

/// Ring capacity: beyond this the oldest audio is dropped to bound latency.
const CAPACITY_SECONDS: f64 = 0.15;
/// Playback starts (and restarts after an underrun) once this much is queued.
const PREFILL_SECONDS: f64 = 0.04;

struct Inner {
    /// Interleaved stereo `f32`; always an even number of samples.
    buf: VecDeque<f32>,
    capacity: usize,
    prefill: usize,
    /// False while waiting to (re)accumulate `prefill` samples.
    primed: bool,
}

/// Shared producer/consumer queue of interleaved stereo samples.
pub struct AudioRing {
    inner: Mutex<Inner>,
}

impl AudioRing {
    /// A ring sized for a device running at `sample_rate` Hz.
    pub fn new(sample_rate: u32) -> Self {
        let frames = |secs: f64| ((f64::from(sample_rate) * secs) as usize).max(1) * 2;
        Self::with_sizes(frames(CAPACITY_SECONDS), frames(PREFILL_SECONDS))
    }

    /// Sizes are in samples (2 per stereo frame); rounded down to even.
    fn with_sizes(capacity: usize, prefill: usize) -> Self {
        let capacity = (capacity & !1).max(2);
        Self {
            inner: Mutex::new(Inner {
                buf: VecDeque::with_capacity(capacity),
                capacity,
                prefill: (prefill & !1).min(capacity),
                primed: false,
            }),
        }
    }

    /// Queue interleaved stereo samples, discarding the oldest on overflow.
    pub fn push(&self, samples: &[f32]) {
        let samples = &samples[..samples.len() & !1];
        let Ok(mut r) = self.inner.lock() else { return };
        let cap = r.capacity;
        let samples = &samples[samples.len().saturating_sub(cap)..];
        let overflow = (r.buf.len() + samples.len()).saturating_sub(cap);
        r.buf.drain(..overflow);
        r.buf.extend(samples.iter().copied());
    }

    /// Drop everything queued and wait for a fresh prefill (pause, fast-forward).
    pub fn clear(&self) {
        if let Ok(mut r) = self.inner.lock() {
            r.buf.clear();
            r.primed = false;
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().buf.len()
    }

    /// Fill a device buffer of `channels`-wide frames. Underrun (or a
    /// contended lock) yields silence rather than blocking the audio thread.
    pub fn fill<T: Sample + FromSample<f32>>(&self, out: &mut [T], channels: usize) {
        if channels == 0 {
            return;
        }
        let mut guard = match self.inner.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => {
                out.fill(T::EQUILIBRIUM);
                return;
            }
        };
        let r = &mut *guard;
        if !r.primed {
            if r.buf.len() >= r.prefill.max(2) {
                r.primed = true;
            } else {
                out.fill(T::EQUILIBRIUM);
                return;
            }
        }
        for frame in out.chunks_mut(channels) {
            match (r.buf.pop_front(), r.buf.pop_front()) {
                (Some(l), Some(rt)) => write_frame(frame, l, rt),
                _ => {
                    // Ran dry: silence the rest and re-prime before resuming.
                    r.primed = false;
                    frame.fill(T::EQUILIBRIUM);
                }
            }
        }
    }
}

/// Map one stereo frame onto `frame.len()` output channels: mono is the
/// average, stereo passes through, extra channels are silent.
fn write_frame<T: Sample + FromSample<f32>>(frame: &mut [T], l: f32, r: f32) {
    match frame {
        [] => {}
        [mono] => *mono = T::from_sample((l + r) * 0.5),
        [left, right, rest @ ..] => {
            *left = T::from_sample(l);
            *right = T::from_sample(r);
            rest.fill(T::EQUILIBRIUM);
        }
    }
}

/// A running output stream plus the ring that feeds it.
pub struct AudioOutput {
    pub ring: Arc<AudioRing>,
    pub sample_rate: u32,
    // Kept alive for playback; stays on the thread that created it (`Stream`
    // is `!Send` on some platforms).
    _stream: cpal::Stream,
}

impl AudioOutput {
    /// Open the default output device with its default config.
    pub fn start() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("no default audio output device")?;
        let supported = device
            .default_output_config()
            .map_err(|e| format!("cannot query audio config: {e}"))?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let sample_rate = config.sample_rate;
        let ring = Arc::new(AudioRing::new(sample_rate));

        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, &ring),
            SampleFormat::F64 => build::<f64>(&device, config, &ring),
            SampleFormat::I16 => build::<i16>(&device, config, &ring),
            SampleFormat::U16 => build::<u16>(&device, config, &ring),
            SampleFormat::I32 => build::<i32>(&device, config, &ring),
            SampleFormat::U32 => build::<u32>(&device, config, &ring),
            SampleFormat::I8 => build::<i8>(&device, config, &ring),
            SampleFormat::U8 => build::<u8>(&device, config, &ring),
            other => return Err(format!("unsupported audio sample format {other}")),
        }
        .map_err(|e| format!("cannot open audio stream: {e}"))?;
        stream
            .play()
            .map_err(|e| format!("cannot start audio stream: {e}"))?;
        Ok(Self {
            ring,
            sample_rate,
            _stream: stream,
        })
    }
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    ring: &Arc<AudioRing>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels);
    let ring = Arc::clone(ring);
    device.build_output_stream(
        config,
        move |data: &mut [T], _| ring.fill(data, channels),
        |e: cpal::Error| {
            if !matches!(e.kind(), cpal::ErrorKind::Xrun) {
                eprintln!("audio stream error: {e}");
            }
        },
        None,
    )
}

/// 440 Hz square wave for `--test-pattern`, generated per video frame.
pub struct ToneGen {
    sample_rate: u32,
    phase: f64,
    /// Fractional samples carried between frames.
    carry: f64,
    frames_per_second: f64,
}

impl ToneGen {
    pub fn new(sample_rate: u32, frames_per_second: f64) -> Self {
        Self {
            sample_rate,
            phase: 0.0,
            carry: 0.0,
            frames_per_second,
        }
    }

    /// Interleaved stereo samples for one video frame.
    pub fn next_frame(&mut self) -> Vec<f32> {
        self.carry += f64::from(self.sample_rate) / self.frames_per_second;
        let n = self.carry.floor();
        self.carry -= n;
        let step = 440.0 / f64::from(self.sample_rate);
        let mut out = Vec::with_capacity(n as usize * 2);
        for _ in 0..n as usize {
            let v = if self.phase < 0.5 { 0.1 } else { -0.1 };
            out.extend_from_slice(&[v, v]);
            self.phase = (self.phase + step).fract();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(cap: usize, prefill: usize) -> AudioRing {
        AudioRing::with_sizes(cap, prefill)
    }

    #[test]
    fn silence_until_prefill_then_stereo_passthrough() {
        let r = ring(16, 4);
        let mut out = [9.0f32; 4];
        r.push(&[0.1, 0.2]);
        r.fill(&mut out, 2);
        assert_eq!(out, [0.0; 4], "below prefill: silence");
        r.push(&[0.3, 0.4, 0.5, 0.6]);
        r.fill(&mut out, 2);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn underrun_gives_silence_and_reprimes() {
        let r = ring(16, 4);
        r.push(&[0.1, 0.2, 0.3, 0.4]);
        let mut out = [9.0f32; 8];
        r.fill(&mut out, 2);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4, 0.0, 0.0, 0.0, 0.0]);
        // One stereo frame is below the prefill, so it waits.
        r.push(&[0.7, 0.8]);
        let mut out = [9.0f32; 2];
        r.fill(&mut out, 2);
        assert_eq!(out, [0.0, 0.0]);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn overflow_drops_oldest_in_whole_frames() {
        let r = ring(6, 2);
        r.push(&[1.0, 1.5, 2.0, 2.5]);
        r.push(&[3.0, 3.5, 4.0, 4.5]);
        assert_eq!(r.len(), 6);
        let mut out = [0.0f32; 6];
        r.fill(&mut out, 2);
        assert_eq!(out, [2.0, 2.5, 3.0, 3.5, 4.0, 4.5]);
    }

    #[test]
    fn oversized_push_keeps_newest() {
        let r = ring(4, 2);
        r.push(&[1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 4.0, 4.0]);
        let mut out = [0.0f32; 4];
        r.fill(&mut out, 2);
        assert_eq!(out, [3.0, 3.0, 4.0, 4.0]);
    }

    #[test]
    fn odd_trailing_sample_is_ignored() {
        let r = ring(8, 2);
        r.push(&[1.0, 2.0, 3.0]);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn mono_averages_channels() {
        let r = ring(8, 2);
        r.push(&[0.2, 0.6, -1.0, 1.0]);
        let mut out = [9.0f32; 2];
        r.fill(&mut out, 1);
        assert!((out[0] - 0.4).abs() < 1e-6);
        assert!(out[1].abs() < 1e-6);
    }

    #[test]
    fn surround_puts_stereo_up_front_and_silences_rest() {
        let r = ring(8, 2);
        r.push(&[0.25, -0.25]);
        let mut out = [9.0f32; 6];
        r.fill(&mut out, 6);
        assert_eq!(out, [0.25, -0.25, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn integer_formats_convert_and_unsigned_silence_is_midpoint() {
        let r = ring(8, 2);
        let mut out = [1i16; 4];
        r.fill(&mut out, 2); // not primed
        assert_eq!(out, [0; 4]);
        let mut out = [1u16; 2];
        r.fill(&mut out, 2);
        assert_eq!(out, [32768; 2], "u16 silence is the midpoint, not 0");
        r.push(&[1.0, -1.0, 0.0, 0.0]);
        let mut out = [0i16; 4];
        r.fill(&mut out, 2);
        assert_eq!(out[0], i16::MAX);
        assert!(out[1] <= -i16::MAX);
        assert_eq!(&out[2..], &[0, 0]);
    }

    #[test]
    fn partial_trailing_frame_and_zero_channels_are_safe() {
        let r = ring(8, 2);
        r.push(&[0.5, 0.5, 0.5, 0.5]);
        let mut out = [9.0f32; 5]; // 2 frames of 2 + a 1-wide remainder
        r.fill(&mut out, 2);
        assert_eq!(out, [0.5, 0.5, 0.5, 0.5, 0.0]);
        r.fill(&mut out, 0);
    }

    #[test]
    fn clear_requires_new_prefill() {
        let r = ring(8, 4);
        r.push(&[0.5; 4]);
        r.clear();
        assert_eq!(r.len(), 0);
        r.push(&[0.5; 2]);
        let mut out = [9.0f32; 2];
        r.fill(&mut out, 2);
        assert_eq!(out, [0.0, 0.0]);
    }

    #[test]
    fn sized_for_sample_rate() {
        let r = AudioRing::new(48_000);
        let inner = r.inner.lock().unwrap();
        assert_eq!(inner.capacity, 48_000 * 15 / 100 * 2);
        assert_eq!(inner.prefill, 48_000 * 4 / 100 * 2);
    }

    #[test]
    fn tone_generator_produces_the_right_amount() {
        let mut t = ToneGen::new(48_000, gb_core::FRAMES_PER_SECOND);
        let total: usize = (0..600).map(|_| t.next_frame().len() / 2).sum();
        let expected = 48_000.0 * 600.0 / gb_core::FRAMES_PER_SECOND;
        assert!((total as f64 - expected).abs() <= 1.0, "{total}");
    }
}
