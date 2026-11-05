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

use crate::scheduler::{Event, Scheduler};

/// Cycles between enabling a timer and its first counted tick. GBATEK doesn't
/// give the figure; two cycles is the commonly measured value (DECISIONS #54).
pub const START_DELAY: u64 = 2;

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
}

#[derive(Debug, Clone, Default)]
pub struct Timers {
    pub t: [Timer; 4],
}

impl Timers {
    fn countup(&self, n: usize) -> bool {
        n > 0 && self.t[n].control & 4 != 0
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
        // An overflow is due but not dispatched yet (events run between
        // instructions): fold into the reload period so we never exceed 0xFFFF.
        let period = 0x1_0000 - u64::from(t.reload);
        (u64::from(t.reload) + (total - 0x1_0000) % period) as u16
    }

    /// TMxCNT_H.
    pub fn read_control(&self, n: usize) -> u16 {
        self.t[n].control
    }

    /// Write TMxCNT_L (the reload value) through a byte-lane `mask`.
    pub fn write_reload(&mut self, n: usize, v: u16, mask: u16) {
        let t = &mut self.t[n];
        t.reload = (t.reload & !mask) | (v & mask);
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
        if !timing_changed {
            return;
        }
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
        self.reschedule(n, sched);
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

    /// (Re)schedule timer `n`'s overflow event from its current state.
    fn reschedule(&mut self, n: usize, sched: &mut Scheduler) {
        let ev = Event::TimerOverflow(n as u8);
        let t = &self.t[n];
        if t.enabled() && !self.countup(n) {
            let ticks = 0x1_0000 - u64::from(t.counter);
            sched.schedule_at(ev, t.start + (ticks << t.shift()));
        } else {
            sched.cancel(ev);
        }
    }

    /// Timer `n` overflowed at time `at`: reload it, schedule the next overflow
    /// and report whether its IRQ is enabled. The cascade into timer `n + 1` is
    /// the caller's job (see [`Timers::cascade`]).
    pub fn overflow(&mut self, n: usize, at: u64, sched: &mut Scheduler) -> bool {
        let t = &mut self.t[n];
        t.counter = t.reload;
        t.start = at;
        let irq = t.irq_enabled();
        self.reschedule(n, sched);
        irq
    }

    /// Timer `n - 1` overflowed: tick timer `n` if it counts up. Returns true if
    /// that overflows `n` in turn.
    pub fn cascade(&mut self, n: usize) -> bool {
        if n >= 4 || !self.t[n].enabled() || !self.countup(n) {
            return false;
        }
        let t = &mut self.t[n];
        let (c, wrapped) = t.counter.overflowing_add(1);
        t.counter = c;
        wrapped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(timers: &mut Timers, sched: &mut Scheduler, n: usize, reload: u16, ctl: u16) {
        timers.write_reload(n, reload, 0xFFFF);
        timers.write_control(n, ctl | 0x80, 0xFFFF, sched.now(), sched);
    }

    #[test]
    fn counter_counts_after_the_start_delay() {
        let mut t = Timers::default();
        let mut s = Scheduler::new();
        start(&mut t, &mut s, 0, 0xFFF0, 0);
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
            start(&mut t, &mut s, 1, 0, sel);
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
        assert!(t.overflow(0, when, &mut s), "IRQ bit set");
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
        start(&mut t, &mut s, 2, 100, 0);
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
        start(&mut t, &mut s, 1, 0, 1);
        let ev = s.when(Event::TimerOverflow(1));
        t.write_control(1, 0xC1, 0xFFFF, 1000, &mut s);
        assert_eq!(s.when(Event::TimerOverflow(1)), ev);
        assert!(t.t[1].irq_enabled());
    }

    #[test]
    fn prescaler_change_while_running_keeps_the_count() {
        let mut t = Timers::default();
        let mut s = Scheduler::new();
        start(&mut t, &mut s, 0, 0, 1);
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
        assert!(!t.cascade(1));
        assert_eq!(t.read_counter(1, 12345), 0xFFFF);
        assert!(t.cascade(1), "wraps");
        // Timer 0 ignores the count-up bit.
        t.write_control(0, 0x84, 0xFFFF, 0, &mut s);
        assert!(s.when(Event::TimerOverflow(0)).is_some());
        // A stopped or non-count-up timer does not cascade.
        assert!(!t.cascade(2));
    }

    #[test]
    fn writing_reload_and_start_in_one_word_uses_the_new_reload() {
        let mut t = Timers::default();
        let mut s = Scheduler::new();
        // A 32-bit write is the reload halfword, then the control halfword.
        t.write_reload(3, 0x1234, 0xFFFF);
        t.write_control(3, 0x80, 0xFFFF, 0, &mut s);
        assert_eq!(t.read_counter(3, 0), 0x1234);
    }
}
