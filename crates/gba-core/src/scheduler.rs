//! The master clock and the cycle-stamped event queue. **Contract.**
//!
//! Components never tick per cycle. They schedule an [`Event`] at an absolute
//! timestamp and the bus dispatches it once the clock reaches that time (after the
//! instruction or DMA unit that crossed it). Ties are broken by insertion order, so
//! dispatch order is deterministic.
//!
//! Adding a variant to [`Event`] is allowed (additive); changing the API is a
//! contract change.

use crate::state::{MAX_CLOCK, StateError, check};
use emu_common::state::{StateReader, StateWriter};

/// How far behind the clock an undispatched event can be: events are dispatched
/// after every step, so only at power-on (and after tests that drive the bus
/// directly), but one HLE call may charge up to 16 frames inside a step, so allow
/// that much. Catching up is a few thousand PPU and APU events, plus one event
/// per period of each observed timer: up to one per cycle for a timer on reload
/// 0xFFFF at prescaler 1, so about 4.5 million. That is bounded work, and the
/// same work the timer costs over those cycles when running normally.
const MAX_EVENT_LATE: u64 = crate::bios::SWI_CYCLE_CAP;

/// How far ahead of the clock a timer event (or one nothing schedules yet) can
/// be: a timer on the slowest prescaler (1024 cycles per tick) counting all 65 536
/// ticks, plus its start delay, is 2^26 + 2 cycles. 2^27 leaves room without
/// admitting nonsense.
const MAX_EVENT_AHEAD: u64 = 1 << 27;

/// Something that happens at a point in time. One pending instance per variant
/// (scheduling a variant that is already pending replaces it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The PPU's visible dots end (cycle 960 of a visible line): the line is
    /// drawn from the state as it is now (DECISIONS #71).
    PpuRender,
    /// The HBlank flag, IRQ and DMA trigger of the current line (cycle 1006).
    PpuHBlank,
    /// Line 0 of the first frame starts (power-on, cycle 0): the one line start
    /// no line-end event announces, so the VCount match is checked here.
    PpuFirstLine,
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

impl Event {
    /// The most events pending at once: one per variant (`TimerOverflow` and
    /// `DmaStart` have four each).
    const MAX_PENDING: usize = 14;

    /// The furthest past the clock this event can be scheduled: the PPU's events
    /// are within the current line, the APU pump one interval away (each handler
    /// schedules from its own time, which is not after the clock).
    fn max_ahead(self) -> u64 {
        match self {
            Self::PpuRender | Self::PpuHBlank | Self::PpuFirstLine | Self::PpuLineEnd => {
                u64::from(crate::CYCLES_PER_LINE)
            }
            Self::ApuSample => crate::apu::PUMP_CYCLES,
            Self::TimerOverflow(_) | Self::DmaStart(_) | Self::IrqLine => MAX_EVENT_AHEAD,
        }
    }

    /// `(kind, argument)` as a save state stores the event.
    fn to_id(self) -> (u8, u8) {
        match self {
            Self::PpuRender => (0, 0),
            Self::PpuHBlank => (1, 0),
            Self::PpuFirstLine => (2, 0),
            Self::PpuLineEnd => (3, 0),
            Self::TimerOverflow(n) => (4, n),
            Self::ApuSample => (5, 0),
            Self::DmaStart(n) => (6, n),
            Self::IrqLine => (7, 0),
        }
    }

    /// The inverse of [`Event::to_id`]; `None` for a pair no event has.
    fn from_id(kind: u8, arg: u8) -> Option<Self> {
        Some(match (kind, arg) {
            (0, 0) => Self::PpuRender,
            (1, 0) => Self::PpuHBlank,
            (2, 0) => Self::PpuFirstLine,
            (3, 0) => Self::PpuLineEnd,
            (4, n @ 0..=3) => Self::TimerOverflow(n),
            (5, 0) => Self::ApuSample,
            (6, n @ 0..=3) => Self::DmaStart(n),
            (7, 0) => Self::IrqLine,
            _ => return None,
        })
    }
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

    /// Events pending (test hook).
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.queue.len()
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

    /// Write the clock, the sequence counter and every queue entry exactly as
    /// they are: the order and the tie-breaking `seq` of each entry are what make
    /// a restored machine dispatch events exactly as the original would
    /// (DECISIONS #73).
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.u64(self.now);
        w.u64(self.seq);
        w.u8(self.queue.len() as u8);
        for e in &self.queue {
            let (kind, arg) = e.event.to_id();
            w.u8(kind);
            w.u8(arg);
            w.u64(e.at);
            w.u64(e.seq);
        }
    }

    /// Restore what [`Scheduler::save_state`] wrote. Entries must be in strict
    /// `(at, seq)` order with distinct events (one pending instance per variant),
    /// each `seq` below the counter, each time within [`MAX_EVENT_LATE`] behind the clock
    /// (events due but not yet dispatched) or no further ahead than
    /// the event can be scheduled ([`Event::max_ahead`]); the line end and the APU pump must be pending, or the
    /// machine would never advance a frame.
    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let now = r.u64()?;
        let seq = r.u64()?;
        check(now < MAX_CLOCK && seq < MAX_CLOCK, "scheduler clock")?;
        let n = usize::from(r.u8()?);
        check(n <= Event::MAX_PENDING, "scheduler queue length")?;
        let mut queue: Vec<Entry> = Vec::with_capacity(n);
        for _ in 0..n {
            let (kind, arg) = (r.u8()?, r.u8()?);
            let event = Event::from_id(kind, arg).ok_or(StateError::Invalid("scheduler event"))?;
            let (at, entry_seq) = (r.u64()?, r.u64()?);
            check(entry_seq < seq, "scheduler entry sequence")?;
            check(
                at.saturating_add(MAX_EVENT_LATE) >= now && at <= now + event.max_ahead(),
                "scheduler entry time",
            )?;
            check(
                queue.iter().all(|e| e.event != event),
                "duplicate scheduler event",
            )?;
            check(
                queue.last().is_none_or(|p| (p.at, p.seq) < (at, entry_seq)),
                "scheduler order",
            )?;
            queue.push(Entry {
                at,
                seq: entry_seq,
                event,
            });
        }
        let pending = |ev| queue.iter().any(|e| e.event == ev);
        check(
            pending(Event::PpuLineEnd) && pending(Event::ApuSample),
            "scheduler is missing the line end or the APU pump",
        )?;
        self.now = now;
        self.seq = seq;
        self.queue = queue;
        Ok(())
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
