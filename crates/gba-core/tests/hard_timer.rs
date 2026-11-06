//! Timer event elision and coalescing (DECISIONS #69), driven through the bus the
//! way `sys_*.rs` does. A timer nothing observes schedules no overflow events, and
//! a fast one that is only counted (IRQ flag, cascade) is coalesced; neither may
//! change what the program can see.

mod sys_common;

use gba_core::bus::{Bus, CpuBus};
use gba_core::scheduler::Event;
use sys_common::{N, bus, run};

const TM: u32 = 0x0400_0100;
const IF: u32 = 0x202;
const IRQ: u32 = 0x40;
const COUNT_UP: u32 = 0x04;
const ENABLE: u32 = 0x80;

/// Start timer `n` with `reload` and control `ctl` (enable added), as one 32-bit
/// write (reload first).
fn start(b: &mut Bus, n: u32, reload: u16, ctl: u32) {
    b.write32(TM + 4 * n, u32::from(reload) | (ctl | ENABLE) << 16, N);
}

fn counter(b: &Bus, n: u32) -> u16 {
    b.read_io16(0x100 + 4 * n)
}

fn pending(b: &Bus, n: u8) -> bool {
    b.sched.when(Event::TimerOverflow(n)).is_some()
}

/// A write to the disabled timer 3's control: changes nothing visible, but any
/// timer write first delivers the overflows of coalesced timers.
fn flush(b: &mut Bus) {
    b.write16(TM + 14, 0, N);
    // The write itself took cycles; dispatch what fell due meanwhile.
    run(b, 0);
}

#[test]
fn an_unobserved_storm_schedules_no_events() {
    let mut b = bus();
    for n in 0..4 {
        start(&mut b, n, 0xFFF0, 0);
    }
    for n in 0..4 {
        assert!(!pending(&b, n as u8), "timer {n}");
    }
    run(&mut b, 1_000_000);
    for n in 0..4 {
        assert!(!pending(&b, n as u8), "timer {n}");
    }
    // The counter is still exact: a reload period of 16 cycles from the start.
    for n in 0..4u32 {
        let start = b.timers.t[n as usize].start;
        let ticks = b.sched.now() - start;
        let want = 0xFFF0 + (ticks - 16) % 16;
        assert_eq!(u64::from(counter(&b, n)), want, "timer {n}");
    }
}

#[test]
fn enabling_the_irq_on_a_running_unobserved_timer_keeps_the_count_and_fires() {
    let mut b = bus();
    start(&mut b, 0, 0x0000, 0);
    run(&mut b, 5_000);
    let (t, before) = (b.sched.now(), counter(&b, 0));
    b.write16(TM + 2, (IRQ | ENABLE) as u16, N);
    let dt = b.sched.now() - t;
    assert_eq!(
        u64::from(counter(&b, 0)),
        u64::from(before) + dt,
        "not disturbed"
    );
    assert!(pending(&b, 0));
    // The first overflow lands where the undisturbed counter says it does.
    let left = 0x1_0000 - u64::from(counter(&b, 0));
    let due = b.sched.when(Event::TimerOverflow(0)).unwrap();
    assert_eq!(due, b.sched.now() + left);
    run(&mut b, left);
    assert_ne!(b.irq.if_ & 1 << 3, 0, "timer 0 IRQ");
}

#[test]
fn a_reload_write_applies_from_the_next_overflow_even_after_a_long_elision() {
    let mut b = bus();
    start(&mut b, 2, 0x8000, 0);
    run(&mut b, 100_000); // three reload periods with no event
    let (t, before) = (b.sched.now(), u64::from(counter(&b, 2)));
    b.write16(TM + 8, 0xF000, N);
    let dt = b.sched.now() - t;
    // The elapsed periods keep the old reload.
    let want = 0x8000 + (before - 0x8000 + dt) % 0x8000;
    assert_eq!(u64::from(counter(&b, 2)), want);
    // The next overflow reloads onto the new value.
    let left = 0x1_0000 - want;
    run(&mut b, left + 10);
    assert_eq!(counter(&b, 2), 0xF000 + 10);
}

/// Everything a program can read from timers 0..=2 and IF.
fn snapshot(b: &Bus) -> [u16; 4] {
    [counter(b, 0), counter(b, 1), counter(b, 2), b.irq.if_]
}

/// Run `setup` on a coalescing bus and on a reference bus where timer 0 clocks
/// a FIFO in use (so it keeps one event per overflow; nothing is written to the
/// FIFO, so the pops change nothing) and compare what a program can read at
/// checkpoints. Returns the coalesced bus.
fn compare_with_per_event_run(setup: impl Fn(&mut Bus)) -> Bus {
    let mut batched = bus();
    setup(&mut batched);
    let mut exact = bus();
    let now = exact.sched.now();
    exact.timers.set_sound([true, false], now, &mut exact.sched);
    setup(&mut exact);
    assert!(pending(&exact, 0));
    for step in 1..=30 {
        for b in [&mut batched, &mut exact] {
            flush(b);
            b.write16(0x0400_0000 + IF, 0xFFFF, N);
        }
        for _ in 0..(10_000 / 4) {
            run(&mut batched, 4);
            run(&mut exact, 4);
        }
        flush(&mut batched);
        flush(&mut exact);
        assert_eq!(snapshot(&batched), snapshot(&exact), "checkpoint {step}");
    }
    assert_eq!(exact.timers.t[0].batch, 1);
    batched
}

#[test]
fn coalesced_cascade_chain_matches_the_per_event_run() {
    // Timer 0 overflows every 16 cycles, timer 1 counts it (wraps every 2), timer 2
    // counts timer 1 and has no IRQ; timer 1 has one.
    let b = compare_with_per_event_run(|b| {
        start(b, 0, 0xFFF0, 0);
        start(b, 1, 0xFFFE, COUNT_UP | IRQ);
        start(b, 2, 0xFF00, COUNT_UP);
    });
    assert!(
        b.timers.t[0].batch > 1,
        "coalesced: {}",
        b.timers.t[0].batch
    );
}

#[test]
fn a_batch_that_wraps_a_follower_many_times_matches_the_per_event_run() {
    // Timer 0 overflows every cycle; timer 1 (reload 0xFFFF) overflows with it, so
    // one batch of 256 wraps it 256 times; timer 2 counts those.
    let b = compare_with_per_event_run(|b| {
        start(b, 0, 0xFFFF, 0);
        start(b, 1, 0xFFFF, COUNT_UP);
        start(b, 2, 0xFFF0, COUNT_UP | IRQ);
    });
    assert!(b.timers.t[0].batch >= 256);
}

#[test]
fn an_irq_storm_still_sets_if_with_few_events() {
    let mut b = bus();
    start(&mut b, 0, 0xFFFF, IRQ);
    let mut dispatches = 0;
    let mut last = b.timers.t[0].start;
    let frames = 10;
    for _ in 0..(frames * 280_896 / 4) {
        run(&mut b, 4);
        if b.timers.t[0].start != last {
            last = b.timers.t[0].start;
            dispatches += 1;
        }
    }
    // One event per overflow would be 2.8 million.
    assert!(dispatches < 20_000, "{dispatches} events");
    b.write16(0x0400_0000 + IF, 0xFFFF, N);
    assert_eq!(b.irq.if_ & 1 << 3, 0);
    run(&mut b, 600);
    assert_ne!(b.irq.if_ & 1 << 3, 0, "IF is raised again within a batch");
}

#[test]
fn a_few_fast_overflows_keep_their_exact_timing() {
    // A short delay: the IRQ must arrive at the first overflow, not a batch later.
    let mut b = bus();
    start(&mut b, 0, 0xFFF0, IRQ);
    let first = b.sched.when(Event::TimerOverflow(0)).unwrap();
    assert_eq!(first, b.timers.t[0].start + 16);
    let wait = first - b.sched.now();
    run(&mut b, wait);
    assert_ne!(b.irq.if_ & 1 << 3, 0);
}

#[test]
fn fifo_clock_timers_stay_exact() {
    let mut b = bus();
    // Direct Sound A on timer 1, nothing written yet: no timer is observed.
    b.write16(0x0400_0084, 0x80, N);
    b.write16(0x0400_0082, 0x0304 | 0x0400, N);
    start(&mut b, 0, 0xFFFF, 0);
    start(&mut b, 1, 0xFFFF, 0);
    assert!(!pending(&b, 0) && !pending(&b, 1), "no FIFO in use yet");
    // The first word makes FIFO A live: timer 1 clocks it, timer 0 does not.
    b.write32(0x0400_00A0, 0x0102_0304, N);
    assert!(b.timers.sound[1] && !b.timers.sound[0]);
    assert!(pending(&b, 1) && !pending(&b, 0));
    run(&mut b, 10_000);
    assert_eq!(b.timers.t[1].batch, 1, "never coalesced");
    // Moving the select to timer 0 moves the events.
    b.write16(0x0400_0082, 0x0304, N);
    assert!(pending(&b, 0) && !pending(&b, 1));
}

#[test]
fn a_sound_dma_channel_makes_the_fifo_timer_observed() {
    let mut b = bus();
    b.write16(0x0400_0082, 0x0304, N); // A on timer 0
    start(&mut b, 0, 0xFFFF, 0);
    assert!(!pending(&b, 0));
    // DMA1, special start timing, destination FIFO A.
    b.write32(0x0400_00BC, 0x0200_0000, N);
    b.write32(0x0400_00C0, 0x0400_00A0, N);
    b.write32(
        0x0400_00C4,
        4 | u32::from(0x8000u16 | 3 << 12 | 1 << 9) << 16,
        N,
    );
    assert!(pending(&b, 0));
    // Disabling the other direction is not needed: stale events are harmless.
}

#[test]
fn a_counted_timer_is_exact_when_its_follower_clocks_a_fifo() {
    // Timer 1 counts timer 0 and clocks FIFO B: timer 0 must not coalesce.
    let mut b = bus();
    b.write16(0x0400_0082, 0x4000, N); // B on timer 1
    b.write32(0x0400_00A4, 0x0101_0101, N);
    start(&mut b, 0, 0xFFF0, 0);
    start(&mut b, 1, 0xFFFF, COUNT_UP);
    run(&mut b, 20_000);
    assert!(pending(&b, 0));
    assert_eq!(b.timers.t[0].batch, 1);
}
