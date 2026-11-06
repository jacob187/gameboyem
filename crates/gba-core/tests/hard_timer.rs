//! Timer event elision (DECISIONS #69), driven through the bus the way `sys_*.rs`
//! does. A timer nothing observes schedules no overflow events, and eliding them
//! may not change what the program can see.

mod sys_common;

use gba_core::bus::{Bus, CpuBus};
use gba_core::scheduler::Event;
use sys_common::{N, bus, run};

const TM: u32 = 0x0400_0100;
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

/// The same programs on a bus whose timers are all observed (IRQ on, so every
/// overflow has an event) and on one whose timers are all elided (IRQ off) read
/// the same counters at every point, across restarts with new reloads and prescalers.
#[test]
fn elided_timers_read_the_same_counters_as_observed_ones() {
    let mut elided = bus();
    let mut observed = bus();
    let mut step = 0u64;
    for round in 0..40u32 {
        for (b, irq) in [(&mut elided, 0u32), (&mut observed, IRQ)] {
            for n in 0..4 {
                let reload = 0xFF00 | (round * 7 + n * 13) as u16;
                // Stopped while the reload changes, so no overflow is in flight.
                b.write16(TM + 4 * n + 2, 0, N);
                b.write16(TM + 4 * n, reload, N);
                b.write16(TM + 4 * n + 2, ((round + n) % 4 | irq | ENABLE) as u16, N);
            }
        }
        for _ in 0..300 {
            step += 1;
            run(&mut elided, 3 + step % 5);
            run(&mut observed, 3 + step % 5);
            if step % 17 == 0 {
                for n in 0..4 {
                    assert_eq!(counter(&elided, n), counter(&observed, n), "step {step}");
                }
            }
        }
        assert!((0..4).all(|n| !pending(&elided, n)));
    }
}

#[test]
fn a_few_fast_overflows_keep_their_exact_timing() {
    // A short delay: the IRQ must arrive at the first overflow, not later.
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
}

#[test]
fn a_reload_write_does_not_swallow_an_overflow_that_is_due_but_not_dispatched() {
    let mut b = bus();
    start(&mut b, 0, 0xFFF0, IRQ);
    // Time passes past the first overflow without the bus dispatching events (a
    // long instruction, an HLE SWI), then the program writes the reload.
    b.idle(30);
    b.write16(TM, 0xFF00, N);
    run(&mut b, 0);
    assert_ne!(b.irq.if_ & 1 << 3, 0, "the overflow still fires");
}
