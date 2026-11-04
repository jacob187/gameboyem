//! Frame pacing: converts elapsed wall-clock time into a whole number of
//! emulated frames, independent of the monitor's refresh rate.

use std::time::Duration;

/// Most frames run per `advance`; beyond this the backlog is dropped so a
/// stall (debugger, laptop sleep, slow frame) cannot cause a catch-up spiral.
pub const MAX_CATCHUP_FRAMES: u32 = 4;

#[derive(Debug)]
pub struct FramePacer {
    /// Seconds of emulated time per frame.
    period: f64,
    /// Emulated seconds owed but not yet run.
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

    /// Drop any accumulated time (after pause, focus loss, ...).
    pub fn reset(&mut self) {
        self.acc = 0.0;
    }

    /// Add `dt` of real time; returns how many frames to run now.
    pub fn advance(&mut self, dt: Duration) -> u32 {
        let cap = f64::from(MAX_CATCHUP_FRAMES) * self.period;
        self.acc = (self.acc + dt.as_secs_f64() * self.speed).min(cap);
        let n = (self.acc / self.period).floor();
        self.acc -= n * self.period;
        n as u32
    }

    /// Real time until the next frame is due.
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

    #[test]
    fn native_rate_on_a_60hz_display() {
        let mut p = FramePacer::new(FPS);
        let frames: u64 = (0..6000)
            .map(|_| u64::from(p.advance(secs(1.0 / 60.0))))
            .sum();
        assert!((frames as f64 - FPS * 100.0).abs() <= 1.0, "{frames}");
    }

    #[test]
    fn native_rate_on_a_144hz_display() {
        let mut p = FramePacer::new(FPS);
        let frames: u64 = (0..14_400)
            .map(|_| u64::from(p.advance(secs(1.0 / 144.0))))
            .sum();
        assert!((frames as f64 - FPS * 100.0).abs() <= 1.0, "{frames}");
    }

    #[test]
    fn exact_period_yields_one_frame() {
        let mut p = FramePacer::new(60.0);
        assert_eq!(p.advance(secs(1.0 / 60.0)), 1);
        assert_eq!(p.advance(Duration::from_millis(1)), 0);
    }

    #[test]
    fn catch_up_is_capped_and_backlog_dropped() {
        let mut p = FramePacer::new(FPS);
        assert_eq!(p.advance(Duration::from_secs(10)), MAX_CATCHUP_FRAMES);
        // The backlog is gone: the next short tick does not burst.
        assert!(p.advance(Duration::from_millis(1)) <= 1);
    }

    #[test]
    fn speed_multiplies_frame_rate_and_rejects_bad_values() {
        let mut p = FramePacer::new(60.0);
        p.set_speed(2.0);
        let frames: u32 = (0..60).map(|_| p.advance(secs(1.0 / 60.0))).sum();
        assert!((119..=121).contains(&frames), "{frames}");
        p.set_speed(f64::NAN);
        p.set_speed(-1.0);
        p.set_speed(0.0);
        p.reset();
        assert_eq!(p.advance(secs(1.0 / 60.0)), 2, "speed must still be 2x");
    }

    #[test]
    fn reset_clears_accumulator() {
        let mut p = FramePacer::new(60.0);
        p.advance(secs(0.9 / 60.0));
        p.reset();
        assert_eq!(p.advance(secs(0.5 / 60.0)), 0);
    }

    #[test]
    fn time_until_next_shrinks_as_time_accrues() {
        let mut p = FramePacer::new(60.0);
        let full = p.time_until_next();
        p.advance(Duration::from_millis(10));
        assert!(p.time_until_next() < full);
        assert!(p.time_until_next() <= secs(1.0 / 60.0));
    }
}
