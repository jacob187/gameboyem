//! Frame pacing: converts elapsed wall-clock time into emulated time to run,
//! independent of the monitor's refresh rate.
//!
//! The pacer is charged in *emulated seconds*, not frames. One
//! `GameBoy::step_frame` normally covers one frame, but a CGB speed switch
//! (KEY1 + STOP) pauses the CPU for 32768 M-cycles inside a single CPU step, so
//! a call can return after 1.6-1.9 frames of emulated time (or, right after,
//! after only a fraction of one). Counting calls would run such a ROM fast and
//! pile audio up in the ring; instead the caller reports how much emulated time
//! each step really covered and the pacer keeps the books.

use std::time::Duration;

/// Most emulated time (in frame periods) owed at once; beyond this the backlog
/// is dropped so a stall (debugger, laptop sleep, slow frame) cannot cause a
/// catch-up spiral.
pub const MAX_CATCHUP_FRAMES: u32 = 4;

/// Hard bound on steps per catch-up, so a step that covers no time (the core
/// returns at once when a frame was already pending) cannot spin the loop.
pub const MAX_STEPS_PER_ADVANCE: u32 = 4 * MAX_CATCHUP_FRAMES;

/// Real seconds of emulated time that `samples` interleaved stereo `f32`
/// values (as drained from the core after one step) represent at `sample_rate`.
///
/// The APU emits samples from real-time M-cycles: in CGB double speed it is
/// clocked at half the CPU rate and the speed-switch pause is ticked at the
/// new speed, so the sample count is real time by construction. That makes it
/// the one measure that stays right for a step spanning a switch. The
/// `mcycles()` delta cannot do that: it mixes pre-switch cycles at the old
/// speed with the pause at the new one, and sampling `double_speed()` before
/// and after the step cannot say where in the step the switch happened. The
/// sample count is quantised to 1/rate, but the APU's accumulator carries the
/// remainder from step to step, so the total never drifts.
pub fn audio_seconds(samples: usize, sample_rate: u32) -> f64 {
    (samples / 2) as f64 / f64::from(sample_rate.max(1))
}

#[derive(Debug)]
pub struct FramePacer {
    /// Seconds of emulated time per normal frame.
    period: f64,
    /// Emulated seconds owed but not yet run. Negative after a step that ran
    /// longer than the time available (it is paid back by later real time).
    acc: f64,
    speed: f64,
}

impl FramePacer {
    pub fn new(fps: f64) -> Self {
        Self {
            period: 1.0 / fps,
            acc: 0.0,
            speed: 1.0,
        }
    }

    /// Speed multiplier (1.0 = real time). Non-finite or non-positive values are ignored.
    pub fn set_speed(&mut self, speed: f64) {
        if speed.is_finite() && speed > 0.0 {
            self.speed = speed;
        }
    }

    /// Drop any accumulated time or debt (after pause, focus loss, ...).
    pub fn reset(&mut self) {
        self.acc = 0.0;
    }

    /// Seconds of emulated time in one normal frame.
    pub fn period(&self) -> f64 {
        self.period
    }

    /// Add `dt` of real time to the emulated time owed.
    ///
    /// At most [`MAX_CATCHUP_FRAMES`] periods are added at once, so after a
    /// stall that many frames run and the rest is dropped. The cap is on the
    /// increment, not the total: the part-period left from the last tick (or
    /// the debt a long step ran up) is real and is kept, so fast-forward over
    /// variable-length steps does not lose time to the cap.
    pub fn advance(&mut self, dt: Duration) {
        let cap = f64::from(MAX_CATCHUP_FRAMES) * self.period;
        let added = (dt.as_secs_f64() * self.speed).min(cap);
        // The outer bound only matters if the step limit left a backlog behind.
        self.acc = (self.acc + added).min(cap + self.period);
    }

    /// A whole frame period is owed: run a step.
    pub fn frame_due(&self) -> bool {
        self.acc >= self.period
    }

    /// Charge `emulated_secs` of emulated time for a step just run. Pass
    /// [`FramePacer::period`] for a step known to cover exactly one frame.
    /// Non-finite or negative values are ignored.
    pub fn consume(&mut self, emulated_secs: f64) {
        if emulated_secs.is_finite() && emulated_secs > 0.0 {
            self.acc -= emulated_secs;
        }
    }

    /// Real time until the next step is due.
    pub fn time_until_next(&self) -> Duration {
        Duration::from_secs_f64(((self.period - self.acc) / self.speed).max(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: f64 = gb_core::FRAMES_PER_SECOND;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    /// Add `dt`, then run nominal one-frame steps while due (what the old
    /// count-based pacer returned).
    fn run(p: &mut FramePacer, dt: Duration) -> u32 {
        p.advance(dt);
        let mut n = 0;
        while p.frame_due() && n < MAX_STEPS_PER_ADVANCE {
            p.consume(p.period());
            n += 1;
        }
        n
    }

    /// Like `run` but each step covers the next of `steps` frame periods
    /// (cycled); returns the steps run and the emulated seconds they covered.
    fn run_steps(p: &mut FramePacer, dt: Duration, steps: &[f64], at: &mut usize) -> (u32, f64) {
        p.advance(dt);
        let (mut n, mut total) = (0, 0.0);
        while p.frame_due() && n < MAX_STEPS_PER_ADVANCE {
            let s = steps[*at % steps.len()] * p.period();
            *at += 1;
            p.consume(s);
            total += s;
            n += 1;
        }
        (n, total)
    }

    #[test]
    fn native_rate_on_a_60hz_display() {
        let mut p = FramePacer::new(FPS);
        let frames: u64 = (0..6000)
            .map(|_| u64::from(run(&mut p, secs(1.0 / 60.0))))
            .sum();
        assert!((frames as f64 - FPS * 100.0).abs() <= 1.0, "{frames}");
    }

    #[test]
    fn native_rate_on_a_144hz_display() {
        let mut p = FramePacer::new(FPS);
        let frames: u64 = (0..14_400)
            .map(|_| u64::from(run(&mut p, secs(1.0 / 144.0))))
            .sum();
        assert!((frames as f64 - FPS * 100.0).abs() <= 1.0, "{frames}");
    }

    #[test]
    fn exact_period_yields_one_frame() {
        let mut p = FramePacer::new(60.0);
        assert_eq!(run(&mut p, secs(1.0 / 60.0)), 1);
        assert_eq!(run(&mut p, Duration::from_millis(1)), 0);
    }

    #[test]
    fn catch_up_is_capped_and_backlog_dropped() {
        let mut p = FramePacer::new(FPS);
        assert_eq!(run(&mut p, Duration::from_secs(10)), MAX_CATCHUP_FRAMES);
        // The backlog is gone: the next short tick does not burst.
        assert!(run(&mut p, Duration::from_millis(1)) <= 1);
    }

    #[test]
    fn speed_multiplies_frame_rate_and_rejects_bad_values() {
        let mut p = FramePacer::new(60.0);
        p.set_speed(2.0);
        let frames: u32 = (0..60).map(|_| run(&mut p, secs(1.0 / 60.0))).sum();
        assert!((119..=121).contains(&frames), "{frames}");
        p.set_speed(f64::NAN);
        p.set_speed(-1.0);
        p.set_speed(0.0);
        p.reset();
        assert_eq!(run(&mut p, secs(1.0 / 60.0)), 2, "speed must still be 2x");
    }

    #[test]
    fn reset_clears_accumulator() {
        let mut p = FramePacer::new(60.0);
        run(&mut p, secs(0.9 / 60.0));
        p.reset();
        assert_eq!(run(&mut p, secs(0.5 / 60.0)), 0);
    }

    #[test]
    fn time_until_next_shrinks_as_time_accrues() {
        let mut p = FramePacer::new(60.0);
        let full = p.time_until_next();
        run(&mut p, Duration::from_millis(10));
        assert!(p.time_until_next() < full);
        assert!(p.time_until_next() <= secs(1.0 / 60.0));
    }

    #[test]
    fn a_normal_frame_charges_exactly_one_period() {
        let mut p = FramePacer::new(FPS);
        p.advance(secs(1.5 / FPS));
        assert!(p.frame_due());
        p.consume(p.period());
        assert!(!p.frame_due());
        let left = p.acc / p.period();
        assert!((left - 0.5).abs() < 1e-6, "{left}");
    }

    #[test]
    fn a_long_step_charges_its_whole_emulated_time() {
        let mut p = FramePacer::new(60.0);
        p.advance(secs(1.0 / 60.0));
        p.consume(1.7 * p.period()); // a step spanning a speed switch
        // 0.7 periods in debt: not due again until 1.7 periods have passed.
        p.advance(secs(1.6 / 60.0));
        assert!(!p.frame_due());
        p.advance(secs(0.1 / 60.0) + Duration::from_micros(1));
        assert!(p.frame_due());
        // The wait shown to the event loop covers the debt too.
        let mut q = FramePacer::new(60.0);
        q.advance(secs(1.0 / 60.0));
        q.consume(1.7 * q.period());
        let wait = q.time_until_next().as_secs_f64() * 60.0;
        assert!((wait - 1.7).abs() < 1e-6, "{wait}");
    }

    #[test]
    fn a_short_step_charges_only_its_fraction() {
        let mut p = FramePacer::new(60.0);
        p.advance(secs(1.0 / 60.0));
        p.consume(0.37 * p.period());
        assert!((p.acc / p.period() - 0.63).abs() < 1e-6, "{}", p.acc);
        // The next step is due after the 0.37 periods just run have passed.
        let wait = p.time_until_next().as_secs_f64() * 60.0;
        assert!((wait - 0.37).abs() < 1e-6, "{wait}");
    }

    #[test]
    fn mixed_steps_never_drift_from_real_time() {
        // A toggling ROM: 1.71, 1.63, 0.37 frames, then normal ones. Over 100
        // simulated seconds the emulated time run matches real time to
        // within one step.
        let steps = [1.71, 1.63, 0.37, 1.0, 1.0, 1.9, 0.1];
        let mut p = FramePacer::new(FPS);
        let (mut at, mut emulated) = (0, 0.0);
        for _ in 0..(100 * 144) {
            emulated += run_steps(&mut p, secs(1.0 / 144.0), &steps, &mut at).1;
        }
        let drift = (emulated - 100.0).abs();
        assert!(drift <= 1.9 * p.period(), "drift {drift} s");
    }

    #[test]
    fn a_ceaseless_switcher_is_not_run_fast() {
        // Every step 1.9 frames long: only 1/1.9 as many steps per second.
        let mut p = FramePacer::new(FPS);
        let (mut at, mut steps) = (0, 0u32);
        for _ in 0..6000 {
            steps += run_steps(&mut p, secs(1.0 / 60.0), &[1.9], &mut at).0;
        }
        let expected = FPS * 100.0 / 1.9;
        assert!((f64::from(steps) - expected).abs() <= 2.0, "{steps}");
    }

    #[test]
    fn fast_forward_runs_variable_steps_at_the_multiple() {
        let mut p = FramePacer::new(FPS);
        p.set_speed(4.0);
        let (mut at, mut emulated) = (0, 0.0);
        for _ in 0..6000 {
            emulated += run_steps(&mut p, secs(1.0 / 60.0), &[1.71, 0.37, 1.0], &mut at).1;
        }
        // 100 s of real time at 4x.
        assert!((emulated - 400.0).abs() <= 1.9 * p.period(), "{emulated}");
    }

    #[test]
    fn zero_length_steps_cannot_spin_the_catch_up() {
        let mut p = FramePacer::new(FPS);
        let mut at = 0;
        let (n, total) = run_steps(&mut p, Duration::from_secs(1), &[0.0], &mut at);
        assert_eq!((n, total), (MAX_STEPS_PER_ADVANCE, 0.0));
    }

    #[test]
    fn bad_charges_are_ignored() {
        let mut p = FramePacer::new(60.0);
        p.advance(secs(1.0 / 60.0));
        p.consume(f64::NAN);
        p.consume(-1.0);
        p.consume(f64::INFINITY);
        assert!(p.frame_due());
    }

    #[test]
    fn audio_seconds_is_real_time() {
        // 2 values per stereo frame; 48000 frames is one second.
        assert_eq!(audio_seconds(96_000, 48_000), 1.0);
        assert_eq!(audio_seconds(0, 48_000), 0.0);
        // A 1.7-frame step at 48 kHz is about 1366 stereo frames.
        let s = audio_seconds(2 * 1366, 48_000);
        assert!((s * FPS - 1.7).abs() < 0.01, "{}", s * FPS);
        assert!(audio_seconds(2, 0).is_finite());
    }
}
