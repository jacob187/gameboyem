//! The four timers (0x0400_0100..0x0400_0110), prescalers and cascade. Owned by
//! P1-SYS. GBATEK "GBA Timers".
//!
//! Nothing is ticked. A running timer remembers the time `start` at which its
//! `counter` was valid and counts `(now - start) >> shift` prescaled ticks since;
//! the live value is computed on read. Its overflow is an `Event::TimerOverflow(n)`
//! scheduled for the exact cycle, rescheduled from the overflow's own timestamp
//! so lateness never accumulates. Count-up (cascade) timers have no event: the
//! previous timer's overflow increments them. On an overflow of timer 0 or 1 the
//! bus also tells the APU (`Apu::on_timer_overflow`), which may request a FIFO DMA.
//!
//! Overflow events exist only for timers something observes, and fast ones are
//! coalesced (DECISIONS #69). A timer is observed when its IRQ is enabled, a
//! running count-up timer follows it, or it clocks a Direct Sound FIFO that is in
//! use ([`Timers::sound`]). An unobserved timer has no event: `read_counter`
//! folds the elapsed time, and [`Timers::sync`] catches `counter`/`start` up
//! (keeping the prescaler phase) when it becomes observed or its reload changes.
//! A timer that overflows faster than [`COALESCE_BELOW`] cycles, and whose
//! overflows are only counted (IRQ flag, cascade), gets one event per `batch`
//! overflows after a short run at full rate. Whoever changes any state that
//! decides this must first call `Bus::flush_timers` so the batched overflows are delivered.

use crate::scheduler::{Event, Scheduler};

/// Cycles between enabling a timer and its first counted tick. GBATEK doesn't
/// give the figure; two cycles is the commonly measured value (DECISIONS #54).
pub const START_DELAY: u64 = 2;

/// A timer whose overflow period is shorter than this many cycles is coalesced
/// (when nothing needs each overflow on its own): one event per at least this
/// many cycles. The IRQ flag and the cascade arrive up to this much late.
pub const COALESCE_BELOW: u64 = 256;

/// Events dispatched at full rate before a fast timer is coalesced, so a short
/// delay (a handful of overflows) keeps its exact timing.
pub const COALESCE_AFTER: u8 = 16;

const PRESCALER_SHIFT: [u32; 4] = [0, 6, 8, 10];

#[derive(Debug, Clone, Copy, Default)]
pub struct Timer {
    pub reload: u16,
    /// TMxCNT_H bits 0-2 (prescaler, count-up), 6 (IRQ) and 7 (enable).
    pub control: u16,
    /// Counter value at time `start` (the frozen value while stopped; the
    /// current value for a count-up timer).
    pub counter: u16,
    pub start: u64,
    /// Overflows the pending event stands for; 0 or 1 is one event per overflow.
    pub batch: u32,
    /// Consecutive events dispatched for a period under [`COALESCE_BELOW`].
    pub streak: u8,
}

impl Timer {
    pub fn enabled(&self) -> bool {
        self.control & 0x80 != 0
    }

    pub fn irq_enabled(&self) -> bool {
        self.control & 0x40 != 0
    }

    fn shift(&self) -> u32 {
        PRESCALER_SHIFT[usize::from(self.control & 3)]
    }

    /// Prescaled ticks from a reload to the next overflow.
    fn reload_ticks(&self) -> u64 {
        0x1_0000 - u64::from(self.reload)
    }

    /// Cycles between two overflows once the counter has reloaded.
    fn period(&self) -> u64 {
        self.reload_ticks() << self.shift()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Timers {
    pub t: [Timer; 4],
    /// Timers 0 and 1: the timer clocks a Direct Sound FIFO that is in use, so
    /// each of its overflows pops a sample. Derived by the bus from SOUNDCNT_H,
    /// the FIFO writes and the DMA channels; set through [`Timers::set_sound`].
    pub sound: [bool; 2],
    /// A word has been written to FIFO A / B (sticky): the FIFO may hold data.
    pub fifo_written: [bool; 2],
}

impl Timers {
    fn countup(&self, n: usize) -> bool {
        n > 0 && self.t[n].control & 4 != 0
    }

    /// A running count-up timer follows timer `n`.
    fn cascades_into(&self, n: usize) -> bool {
        n < 3 && self.t[n + 1].enabled() && self.countup(n + 1)
    }

    /// Something needs timer `n`'s overflows: its IRQ, a count-up follower, or a
    /// FIFO it clocks.
    fn observed(&self, n: usize) -> bool {
        self.t[n].irq_enabled() || (n < 2 && self.sound[n]) || self.cascades_into(n)
    }

    /// Every overflow of timer `n` must be handled on its own, at its exact time:
    /// it clocks a FIFO, directly or through the count-up chain.
    fn needs_exact(&self, n: usize) -> bool {
        (n < 2 && self.sound[n]) || (self.cascades_into(n) && self.needs_exact(n + 1))
    }

    /// How many overflows one event of timer `n` stands for.
    fn batch_size(&self, n: usize) -> u32 {
        let t = &self.t[n];
        let period = t.period();
        if t.streak < COALESCE_AFTER || period >= COALESCE_BELOW || self.needs_exact(n) {
            return 1;
        }
        COALESCE_BELOW.div_ceil(period) as u32
    }

    /// TMxCNT_L: the live counter.
    pub fn read_counter(&self, n: usize, now: u64) -> u16 {
        let t = &self.t[n];
        if !t.enabled() || self.countup(n) {
            return t.counter;
        }
        let ticks = now.saturating_sub(t.start) >> t.shift();
        let total = u64::from(t.counter) + ticks;
        if total <= 0xFFFF {
            return total as u16;
        }
        // An overflow is due but not dispatched (events run between instructions)
        // or has no event at all: fold into the reload period so we never exceed
        // 0xFFFF.
        let period = t.reload_ticks();
        (u64::from(t.reload) + (total - 0x1_0000) % period) as u16
    }

    /// Overflows a running prescaled timer has made since its `counter`/`start`
    /// point, up to `now`. Zero for a stopped or count-up timer.
    pub fn pending_overflows(&self, n: usize, now: u64) -> u32 {
        let t = &self.t[n];
        if !t.enabled() || self.countup(n) {
            return 0;
        }
        let ticks = now.saturating_sub(t.start) >> t.shift();
        let total = u64::from(t.counter) + ticks;
        if total <= 0xFFFF {
            return 0;
        }
        (1 + (total - 0x1_0000) / t.reload_ticks()) as u32
    }

    /// Move a running prescaled timer's `counter`/`start` forward to the last
    /// prescaler tick at or before `now`, folding any overflows (reload applied)
    /// without delivering them. The phase is kept, so the trajectory is unchanged.
    pub fn sync(&mut self, n: usize, now: u64) {
        if !self.t[n].enabled() || self.countup(n) {
            return;
        }
        let t = &mut self.t[n];
        let ticks = now.saturating_sub(t.start) >> t.shift();
        if ticks == 0 {
            return;
        }
        let total = u64::from(t.counter) + ticks;
        t.counter = if total <= 0xFFFF {
            total as u16
        } else {
            (u64::from(t.reload) + (total - 0x1_0000) % t.reload_ticks()) as u16
        };
        t.start += ticks << t.shift();
    }

    /// TMxCNT_H.
    pub fn read_control(&self, n: usize) -> u16 {
        self.t[n].control
    }

    /// Write TMxCNT_L (the reload value) through a byte-lane `mask`. A running
    /// timer without an event is caught up first, so the elapsed periods keep the
    /// old reload. One with an event needs no catch-up (its first overflow does not
    /// depend on the reload and `overflow` restarts it), and folding would swallow
    /// an overflow that is due but not yet dispatched.
    pub fn write_reload(&mut self, n: usize, v: u16, mask: u16, now: u64, sched: &mut Scheduler) {
        if sched.when(Event::TimerOverflow(n as u8)).is_none() {
            self.sync(n, now);
        }
        let t = &mut self.t[n];
        t.reload = (t.reload & !mask) | (v & mask);
        self.reschedule_all(now, sched);
    }

    /// Write TMxCNT_H through a byte-lane `mask`.
    pub fn write_control(&mut self, n: usize, v: u16, mask: u16, now: u64, sched: &mut Scheduler) {
        let old = self.t[n].control;
        let mut new = (old & !mask) | (v & mask);
        new &= 0xC7;
        if n == 0 {
            new &= !4; // timer 0 has no count-up mode
        }
        let was = old & 0x80 != 0;
        let is = new & 0x80 != 0;
        let timing_changed = (old ^ new) & 0x87 != 0;
        if was && timing_changed {
            self.rebase(n, now);
        }
        self.t[n].control = new;
        if timing_changed {
            self.t[n].streak = 0;
            if was && is && (old ^ new) & 4 != 0 {
                // Count-up -> prescaled: the prescaler starts now.
                self.t[n].start = now;
            }
            if is && !was {
                // Starting reloads the counter; counting begins after the delay.
                let t = &mut self.t[n];
                t.counter = t.reload;
                t.start = now + START_DELAY;
            }
        }
        self.reschedule_all(now, sched);
    }

    /// Bring `counter` up to `now` and restart the prescaler there (the
    /// prescaler's partial count is dropped when the timing bits change).
    fn rebase(&mut self, n: usize, now: u64) {
        if self.countup(n) {
            return;
        }
        let counter = self.read_counter(n, now);
        let t = &mut self.t[n];
        t.counter = counter;
        t.start = now;
    }

    /// Record which timers clock a FIFO in use and reschedule. Flush the batches
    /// first.
    pub fn set_sound(&mut self, sound: [bool; 2], now: u64, sched: &mut Scheduler) {
        self.sound = sound;
        self.reschedule_all(now, sched);
    }

    /// Whether any timer has a coalesced event pending.
    pub fn any_batched(&self) -> bool {
        self.t.iter().any(|t| t.batch > 1)
    }

    /// Bring every timer's event in line with what is observed now.
    pub fn reschedule_all(&mut self, now: u64, sched: &mut Scheduler) {
        for n in 0..4 {
            self.reschedule(n, now, sched);
        }
    }

    /// (Re)schedule timer `n`'s overflow event from its current state, or cancel
    /// it if nothing observes the timer.
    fn reschedule(&mut self, n: usize, now: u64, sched: &mut Scheduler) {
        let ev = Event::TimerOverflow(n as u8);
        if !self.t[n].enabled() || self.countup(n) || !self.observed(n) {
            sched.cancel(ev);
            self.t[n].batch = 0;
            self.t[n].streak = 0;
            return;
        }
        if sched.when(ev).is_none() {
            // Not tracked until now: its counter/start may be far behind.
            self.sync(n, now);
        }
        self.schedule(n, sched);
    }

    /// Schedule the event for timer `n`'s next batch of overflows, leaving an
    /// already correct event (and its place in the tie order) alone.
    fn schedule(&mut self, n: usize, sched: &mut Scheduler) {
        let ev = Event::TimerOverflow(n as u8);
        let k = self.batch_size(n);
        let t = &mut self.t[n];
        let first = t.start + ((0x1_0000 - u64::from(t.counter)) << t.shift());
        let at = first + u64::from(k - 1) * t.period();
        t.batch = k;
        if sched.when(ev) != Some(at) {
            sched.schedule_at(ev, at);
        }
    }

    /// Timer `n`'s event fired at `at` (the last of its `batch` overflows):
    /// reload it and schedule the next. The IRQ and the cascade are the caller's
    /// job (see [`Timers::cascade`]).
    pub fn overflow(&mut self, n: usize, at: u64, sched: &mut Scheduler) {
        let t = &mut self.t[n];
        t.counter = t.reload;
        t.start = at;
        t.streak = if t.period() < COALESCE_BELOW {
            t.streak.saturating_add(1)
        } else {
            0
        };
        self.schedule(n, sched);
    }

    /// Timer `n - 1` overflowed `count` times: advance timer `n` if it counts up.
    /// Returns how many times that overflowed `n` in turn.
    pub fn cascade(&mut self, n: usize, count: u32) -> u32 {
        if n >= 4 || !self.t[n].enabled() || !self.countup(n) {
            return 0;
        }
        let t = &mut self.t[n];
        let total = u64::from(t.counter) + u64::from(count);
        if total <= 0xFFFF {
            t.counter = total as u16;
            return 0;
        }
        let over = total - 0x1_0000;
        t.counter = (u64::from(t.reload) + over % t.reload_ticks()) as u16;
        (1 + over / t.reload_ticks()) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(timers: &mut Timers, sched: &mut Scheduler, n: usize, reload: u16, ctl: u16) {
        timers.write_reload(n, reload, 0xFFFF, sched.now(), sched);
        timers.write_control(n, ctl | 0x80, 0xFFFF, sched.now(), sched);
    }

    #[test]
    fn counter_counts_after_the_start_delay() {
        let mut t = Timers::default();
        let mut s = Scheduler::new();
        start(&mut t, &mut s, 0, 0xFFF0, 0x40);
        assert_eq!(t.read_counter(0, 0), 0xFFF0);
        assert_eq!(t.read_counter(0, START_DELAY), 0xFFF0);
        assert_eq!(t.read_counter(0, START_DELAY + 5), 0xFFF5);
        assert_eq!(s.when(Event::TimerOverflow(0)), Some(START_DELAY + 16));
    }

    #[test]
    fn prescalers_divide() {
        for (sel, shift) in [(1u16, 6u32), (2, 8), (3, 10)] {
            let mut t = Timers::default();
            let mut s = Scheduler::new();
            start(&mut t, &mut s, 1, 0, sel | 0x40);
            let at = START_DELAY + (0x1_0000u64 << shift);
            assert_eq!(s.when(Event::TimerOverflow(1)), Some(at));
            assert_eq!(t.read_counter(1, START_DELAY + (5 << shift) + 3), 5);
        }
    }

    #[test]
    fn overflow_reloads_and_reschedules_from_its_own_time() {
        let mut t = Timers::default();
        let mut s = Scheduler::new();
        start(&mut t, &mut s, 0, 0xFF00, 0x40);
        let at = s.when(Event::TimerOverflow(0)).unwrap();
        assert_eq!(at, START_DELAY + 0x100);
        // Dispatched late: the next overflow is still a whole period after `at`.
        s.advance(at + 40);
        let (ev, when) = s.pop_due().unwrap();
        assert_eq!(ev, Event::TimerOverflow(0));
        t.overflow(0, when, &mut s);
        assert_eq!(s.when(Event::TimerOverflow(0)), Some(at + 0x100));
        assert_eq!(t.read_counter(0, at + 40), 0xFF00 + 40);
    }

    #[test]
    fn a_read_past_an_undispatched_overflow_wraps_into_the_period() {
        let mut t = Timers::default();
        let mut s = Scheduler::new();
        start(&mut t, &mut s, 0, 0xFFFE, 0);
        // Overflow at START_DELAY + 2; read at +5 without dispatching.
        assert_eq!(t.read_counter(0, START_DELAY + 5), 0xFFFF);
    }

    #[test]
    fn stopping_freezes_and_restarting_reloads() {
        let mut t = Timers::default();
        let mut s = Scheduler::new();
        start(&mut t, &mut s, 2, 100, 0x40);
        assert!(s.when(Event::TimerOverflow(2)).is_some());
        t.write_control(2, 0, 0xFFFF, START_DELAY + 7, &mut s);
        assert_eq!(t.read_counter(2, 10_000), 107);
        assert_eq!(s.when(Event::TimerOverflow(2)), None);
        t.write_control(2, 0x80, 0xFFFF, 20_000, &mut s);
        assert_eq!(t.read_counter(2, 20_000), 100);
    }

    #[test]
    fn irq_bit_change_keeps_the_phase() {
        let mut t = Timers::default();
        let mut s = Scheduler::new();
        start(&mut t, &mut s, 1, 0, 0x41);
        let ev = s.when(Event::TimerOverflow(1));
        assert!(ev.is_some());
        // The IRQ goes off: nothing observes the timer, so no event. It comes
        // back at the same time when the IRQ is enabled again.
        t.write_control(1, 0x81, 0xFFFF, 1000, &mut s);
        assert_eq!(s.when(Event::TimerOverflow(1)), None);
        t.write_control(1, 0xC1, 0xFFFF, 2000, &mut s);
        assert_eq!(s.when(Event::TimerOverflow(1)), ev);
        assert!(t.t[1].irq_enabled());
    }

    #[test]
    fn prescaler_change_while_running_keeps_the_count() {
        let mut t = Timers::default();
        let mut s = Scheduler::new();
        start(&mut t, &mut s, 0, 0, 1);
        assert_eq!(s.when(Event::TimerOverflow(0)), None, "unobserved");
        let now = START_DELAY + (3 << 6) + 10;
        t.write_control(0, 0x80, 0xFFFF, now, &mut s);
        assert_eq!(t.read_counter(0, now), 3);
        assert_eq!(t.read_counter(0, now + 4), 7);
    }

    #[test]
    fn cascade_counts_previous_overflows() {
        let mut t = Timers::default();
        let mut s = Scheduler::new();
        start(&mut t, &mut s, 1, 0xFFFE, 4);
        assert!(s.when(Event::TimerOverflow(1)).is_none());
        assert_eq!(t.cascade(1, 1), 0);
        assert_eq!(t.read_counter(1, 12345), 0xFFFF);
        assert_eq!(t.cascade(1, 1), 1, "wraps");
        // Timer 0 ignores the count-up bit; the running count-up timer 1 makes
        // it observed.
        t.write_control(0, 0x84, 0xFFFF, 0, &mut s);
        assert!(s.when(Event::TimerOverflow(0)).is_some());
        // A stopped or non-count-up timer does not cascade.
        assert_eq!(t.cascade(2, 1), 0);
    }

    #[test]
    fn writing_reload_and_start_in_one_word_uses_the_new_reload() {
        let mut t = Timers::default();
        let mut s = Scheduler::new();
        // A 32-bit write is the reload halfword, then the control halfword.
        t.write_reload(3, 0x1234, 0xFFFF, 0, &mut s);
        t.write_control(3, 0x80, 0xFFFF, 0, &mut s);
        assert_eq!(t.read_counter(3, 0), 0x1234);
    }
}
