//! The master clock and the cycle-stamped event queue. **Contract.**
//!
//! Components never tick per cycle. They schedule an [`Event`] at an absolute
//! timestamp and the bus dispatches it once the clock reaches that time (after the
//! instruction or DMA unit that crossed it). Ties are broken by insertion order, so
//! dispatch order is deterministic.
//!
//! Adding a variant to [`Event`] is allowed (additive); changing the API is a
//! contract change.

/// Something that happens at a point in time. One pending instance per variant
/// (scheduling a variant that is already pending replaces it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The PPU reaches dot 240 of the current line (HBlank begins).
    PpuHBlank,
    /// The PPU finishes the current line (VCOUNT advances).
    PpuLineEnd,
    /// Timer `n` (0..=3) overflows.
    TimerOverflow(u8),
    /// The APU's PSG frame sequencer / sample clock.
    ApuSample,
    /// A DMA channel's start delay has elapsed (`n` = 0..=3).
    DmaStart(u8),
    /// A delayed interrupt-flag update becomes visible to the CPU.
    IrqLine,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    at: u64,
    seq: u64,
    event: Event,
}

/// The master clock plus the pending-event queue.
#[derive(Debug, Clone, Default)]
pub struct Scheduler {
    now: u64,
    seq: u64,
    /// Small and kept sorted by `(at, seq)`, so the next event is at index 0. The
    /// GBA has under a dozen pending events at any time, so a sorted Vec beats a heap.
    queue: Vec<Entry>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current time in CPU cycles since power-on.
    #[inline]
    pub fn now(&self) -> u64 {
        self.now
    }

    /// Advance the clock by `cycles` (bus accesses, internal cycles, halt skips).
    #[inline]
    pub fn advance(&mut self, cycles: u64) {
        self.now += cycles;
    }

    /// Schedule `event` `delay` cycles from now, replacing any pending instance.
    pub fn schedule(&mut self, event: Event, delay: u64) {
        self.schedule_at(event, self.now + delay);
    }

    /// Schedule `event` at absolute time `at`, replacing any pending instance.
    pub fn schedule_at(&mut self, event: Event, at: u64) {
        self.cancel(event);
        let entry = Entry {
            at,
            seq: self.seq,
            event,
        };
        self.seq += 1;
        let pos = self
            .queue
            .partition_point(|e| (e.at, e.seq) <= (entry.at, entry.seq));
        self.queue.insert(pos, entry);
    }

    /// Remove `event` if it is pending.
    pub fn cancel(&mut self, event: Event) {
        self.queue.retain(|e| e.event != event);
    }

    /// When `event` is due, if it is pending.
    pub fn when(&self, event: Event) -> Option<u64> {
        self.queue.iter().find(|e| e.event == event).map(|e| e.at)
    }

    /// Timestamp of the next pending event.
    #[inline]
    pub fn next_at(&self) -> Option<u64> {
        self.queue.first().map(|e| e.at)
    }

    /// Pop the next event if it is due (`at <= now`), returning it and its
    /// scheduled time (handlers that reschedule periodically add to `at`, not
    /// `now`, so lateness doesn't accumulate).
    #[inline]
    pub fn pop_due(&mut self) -> Option<(Event, u64)> {
        match self.queue.first() {
            Some(e) if e.at <= self.now => {
                let e = self.queue.remove(0);
                Some((e.event, e.at))
            }
            _ => None,
        }
    }

    /// Jump the clock to the next event (used while the CPU is halted).
    /// Returns false if nothing is pending.
    pub fn skip_to_next(&mut self) -> bool {
        match self.next_at() {
            Some(at) => {
                self.now = self.now.max(at);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_pop_in_time_then_insertion_order() {
        let mut s = Scheduler::new();
        s.schedule(Event::PpuLineEnd, 10);
        s.schedule(Event::TimerOverflow(0), 5);
        s.schedule(Event::TimerOverflow(1), 10);
        assert_eq!(s.pop_due(), None);
        s.advance(10);
        assert_eq!(s.pop_due(), Some((Event::TimerOverflow(0), 5)));
        assert_eq!(s.pop_due(), Some((Event::PpuLineEnd, 10)));
        assert_eq!(s.pop_due(), Some((Event::TimerOverflow(1), 10)));
        assert_eq!(s.pop_due(), None);
    }

    #[test]
    fn rescheduling_replaces_and_cancel_removes() {
        let mut s = Scheduler::new();
        s.schedule(Event::PpuHBlank, 100);
        s.schedule(Event::PpuHBlank, 3);
        assert_eq!(s.when(Event::PpuHBlank), Some(3));
        s.cancel(Event::PpuHBlank);
        assert_eq!(s.next_at(), None);
        assert!(!s.skip_to_next());
    }

    #[test]
    fn skip_to_next_jumps_the_clock() {
        let mut s = Scheduler::new();
        s.schedule(Event::ApuSample, 512);
        assert!(s.skip_to_next());
        assert_eq!(s.now(), 512);
        assert_eq!(s.pop_due(), Some((Event::ApuSample, 512)));
    }
}
