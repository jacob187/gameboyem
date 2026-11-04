//! Cycle-exact tests that drive a real [`Bus`] through [`CpuBus`], the same way
//! the CPU does. Instruction helpers mirror the M-cycle shape of the SM83
//! instructions the mooneye tests use, so the nop counts below are the ones in
//! the mooneye sources (`acceptance/timer/*.s`, `acceptance/oam_dma_*.s`).
//!
//! Convention: "tick N" means the N-th M-cycle after the write access that
//! started the sequence; the access of a cycle sees the state *after* that
//! cycle's component tick.

use super::*;
use crate::cartridge;

const DIV: u16 = 0xFF04;
const TIMA: u16 = 0xFF05;
const TMA: u16 = 0xFF06;
const TAC: u16 = 0xFF07;
const IF: u16 = 0xFF0F;
const DMA: u16 = 0xFF46;

fn new_bus() -> Bus {
    let mut rom = vec![0u8; 0x8000];
    // Distinct ROM bytes so bus-conflict reads are distinguishable from 0xFF.
    rom[0] = 0x5A;
    rom[0x4000] = 0x6B;
    let mut b = Bus::new(cartridge::from_rom(rom).unwrap());
    b.write(0xFF40, 0); // LCD off: OAM/VRAM never PPU-blocked in these tests
    b
}

/// `nops n`
fn nops(b: &mut Bus, n: usize) {
    for _ in 0..n {
        b.tick();
    }
}

/// `ldh (lo), a` : fetch, fetch, write.
fn ldh_w(b: &mut Bus, lo: u16, v: u8) {
    nops(b, 2);
    b.write(0xFF00 | lo, v);
}

/// `ldh a, (lo)` : fetch, fetch, read.
fn ldh_r(b: &mut Bus, lo: u16) -> u8 {
    nops(b, 2);
    b.read(0xFF00 | lo)
}

/// The mooneye timer prelude: 65536 Hz timer, TIMA = TMA = `init`, ending with
/// the DIV write that phases everything that follows (tick 0).
fn timer_prelude(b: &mut Bus, init: u8) {
    ldh_w(b, 0xFF, 0); // IE
    ldh_w(b, 0x0F, 0); // IF
    ldh_w(b, 0x04, 0); // DIV
    ldh_w(b, 0x05, init);
    ldh_w(b, 0x06, init);
    ldh_w(b, 0x07, 0x06);
    ldh_w(b, 0x04, 0);
    ldh_w(b, 0x05, init);
    ldh_w(b, 0x04, 0); // phase reference
}

// ---------------------------------------------------------------- timer ----

/// mooneye `timer/tima_reload`: TIMA is 0 for exactly one M-cycle, then TMA.
#[test]
fn tima_reload_visible_values() {
    let cases = [
        (28, 0xFF),
        (29, 0x00),
        (30, 0xFE),
        (60, 0xFF),
        (61, 0x00),
        (62, 0xFE),
    ];
    for (n, expect) in cases {
        let mut b = new_bus();
        timer_prelude(&mut b, 0xFE);
        nops(&mut b, n);
        assert_eq!(ldh_r(&mut b, 0x05), expect, "after {n} nops");
    }
}

/// The overflow edge: IF is requested on the reload tick, not before.
#[test]
fn timer_interrupt_is_requested_on_reload_cycle() {
    let mut b = new_bus();
    timer_prelude(&mut b, 0xFE);
    // Edges at ticks 16 (FE->FF) and 32 (FF->00); reload + IRQ at tick 33.
    nops(&mut b, 32);
    assert_eq!(b.peek(TIMA), 0x00, "tick 32: TIMA reads 0");
    assert_eq!(b.peek(IF) & 0x04, 0, "tick 32: IF not yet set");
    b.tick();
    assert_eq!(b.peek(TIMA), 0xFE, "tick 33: reloaded");
    assert_eq!(b.peek(IF) & 0x04, 0x04, "tick 33: IF set");
}

/// mooneye `timer/tima_write_reloading`: write at tick 31 (normal), 32
/// (overflow cycle: cancels reload and IRQ), 33 (reload cycle: ignored), 34.
#[test]
fn tima_write_around_overflow() {
    // (nops before write, expected TIMA read 3 cycles later)
    let cases = [(28, 0x80), (29, 0x7F), (30, 0xFE), (31, 0x7F)];
    for (n, expect) in cases {
        let mut b = new_bus();
        timer_prelude(&mut b, 0xFE);
        nops(&mut b, n);
        ldh_w(&mut b, 0x05, 0x7F);
        assert_eq!(ldh_r(&mut b, 0x05), expect, "TIMA write after {n} nops");
    }
}

#[test]
fn tima_write_during_overflow_cycle_cancels_interrupt() {
    let mut b = new_bus();
    timer_prelude(&mut b, 0xFE);
    nops(&mut b, 29 + 2);
    b.write(TIMA, 0x40); // tick 32
    nops(&mut b, 8);
    assert_eq!(b.read(IF) & 0x04, 0, "reload and IRQ were cancelled");
    assert_eq!(b.read(TIMA) & 0xF0, 0x40, "TIMA keeps counting from 0x40");
}

#[test]
fn tima_write_during_reload_cycle_is_ignored_but_irq_still_fires() {
    let mut b = new_bus();
    timer_prelude(&mut b, 0xFE);
    nops(&mut b, 30 + 2);
    b.write(TIMA, 0x40); // tick 33
    assert_eq!(b.read(TIMA), 0xFE);
    assert_eq!(b.read(IF) & 0x04, 0x04);
}

/// mooneye `timer/tma_write_reloading`: TMA written on the overflow cycle is
/// what gets reloaded, and a write on the reload cycle itself also propagates
/// into TIMA; only from the cycle after that is it too late.
#[test]
fn tma_write_around_reload() {
    let cases = [(28, 0x7F), (29, 0x7F), (30, 0x7F), (31, 0xFE)];
    for (n, expect) in cases {
        let mut b = new_bus();
        timer_prelude(&mut b, 0xFE);
        nops(&mut b, n);
        ldh_w(&mut b, 0x06, 0x7F);
        assert_eq!(ldh_r(&mut b, 0x05), expect, "TMA write after {n} nops");
    }
}

#[test]
fn tma_write_on_reload_cycle_propagates_to_tima() {
    let mut b = new_bus();
    timer_prelude(&mut b, 0xFE);
    nops(&mut b, 30 + 2);
    b.write(TMA, 0x11); // tick 33
    assert_eq!(b.read(TIMA), 0x11);
    assert_eq!(b.read(TMA), 0x11);
}

#[test]
fn div_counts_through_the_bus() {
    let mut b = new_bus();
    b.write(DIV, 0x55);
    assert_eq!(b.read(DIV), 0, "write resets, value is ignored");
    nops(&mut b, 61);
    assert_eq!(b.read(DIV), 0, "tick 63: counter 252");
    assert_eq!(b.read(DIV), 1, "tick 64: counter 256");
}

/// DIV write while the selected bit is high clocks TIMA (falling edge).
#[test]
fn div_write_glitch_increments_tima() {
    let mut b = new_bus();
    b.write(DIV, 0);
    b.write(TIMA, 0);
    b.write(TAC, 0x05); // 16 T-cycles: bit 3
    // counter: after the TAC write tick = 8 (bit 3 set)
    assert_eq!(b.timer.div_counter() & 0x08, 0x08);
    b.write(DIV, 0);
    assert_eq!(b.peek(TIMA), 1, "write while bit 3 high is a falling edge");
}

/// DIV write overflowing TIMA goes through the normal reload pipeline.
#[test]
fn div_write_glitch_overflow_reloads_next_cycle() {
    let mut b = new_bus();
    b.write(DIV, 0);
    b.write(TMA, 0x33);
    b.write(TIMA, 0xFF);
    b.write(TAC, 0x05);
    assert_eq!(b.timer.div_counter() & 0x08, 0x08);
    b.write(DIV, 0);
    // Overflow cycle is the write itself; this read is the reload cycle.
    assert_eq!(b.read(TIMA), 0x33);
    assert_eq!(b.read(IF) & 0x04, 0x04);
}

/// TAC write: disabling the timer while the selected bit is high is a falling
/// edge and bumps TIMA.
#[test]
fn tac_disable_glitch() {
    let mut b = new_bus();
    b.write(DIV, 0);
    b.write(TIMA, 0);
    b.write(TAC, 0x05);
    assert_eq!(b.timer.div_counter() & 0x08, 0x08);
    b.write(TAC, 0x01);
    assert_eq!(b.peek(TIMA), 1);
}

/// TAC write: switching to a frequency whose bit is low while the old bit is
/// high is a falling edge; switching to one that is also high is not.
#[test]
fn tac_frequency_switch_glitch() {
    let mut b = new_bus();
    b.write(DIV, 0);
    b.write(TIMA, 0);
    b.write(TAC, 0x05); // bit 3
    assert_eq!(
        b.timer.div_counter() & 0x0F8,
        0x08,
        "bit 3 high, bit 5/7 low"
    );
    b.write(TAC, 0x06); // bit 5 low -> edge
    assert_eq!(b.peek(TIMA), 1);

    let mut b = new_bus();
    b.write(DIV, 0);
    b.write(TIMA, 0);
    b.write(TAC, 0x07); // bit 7
    // run until bits 7 and 3 are both high: counter = 0x88 (+ ticks)
    while b.timer.div_counter() & 0x8C != 0x88 {
        b.tick();
    }
    let tima = b.peek(TIMA);
    b.write(TAC, 0x05); // counter is now 0x8C: bit 3 also high -> no edge
    assert_eq!(b.timer.div_counter() & 0x88, 0x88);
    assert_eq!(b.peek(TIMA), tima);
}

/// Enabling the timer never increments by itself.
#[test]
fn tac_enable_has_no_glitch() {
    let mut b = new_bus();
    b.write(DIV, 0);
    b.write(TIMA, 0);
    nops(&mut b, 1);
    b.write(TAC, 0x05);
    assert_eq!(b.timer.div_counter() & 0x08, 0x08, "signal went high");
    assert_eq!(b.peek(TIMA), 0);
}

#[test]
fn registers_unused_bits() {
    let mut b = new_bus();
    b.write(TAC, 0x00);
    assert_eq!(b.read(TAC), 0xF8);
    b.write(TAC, 0xFF);
    assert_eq!(b.read(TAC), 0xFF);
}

/// STOP resets the divider.
#[test]
fn stop_resets_div() {
    let mut b = new_bus();
    nops(&mut b, 200);
    assert_ne!(b.timer.div_counter(), 0);
    b.stop();
    assert_eq!(b.timer.div_counter(), 0);
}

// ------------------------------------------------------------ IF / IE ------

#[test]
fn if_ie_registers_through_bus() {
    let mut b = new_bus();
    b.write(IF, 0x00);
    assert_eq!(b.read(IF), 0xE0);
    b.write(IF, 0xFF);
    assert_eq!(b.read(IF), 0xFF);
    b.write(IF, 0xE5);
    assert_eq!(b.read(IF), 0xE5);
    b.write(0xFFFF, 0x00);
    assert_eq!(b.read(0xFFFF), 0x00);
    b.write(0xFFFF, 0xFF);
    assert_eq!(b.read(0xFFFF), 0xFF, "IE keeps all 8 bits");
    assert_eq!(b.pending_interrupts(), 0x05);
    b.acknowledge_interrupt(0x01);
    assert_eq!(b.pending_interrupts(), 0x04);
}

#[test]
fn post_boot_interrupt_state() {
    let b = new_bus();
    assert_eq!(b.interrupts.read_if() & 0x1F, 0x01);
    assert_eq!(b.interrupts.read_ie(), 0x00);
}

// ------------------------------------------------------------- OAM DMA -----

fn fill_wram(b: &mut Bus) {
    for i in 0..0x2000usize {
        b.wram[i] = (i as u8).wrapping_mul(7).wrapping_add(1) | 0x01;
    }
}

/// mooneye `oam_dma_timing`: OAM reads are 0xFF through write+161, data from +162.
#[test]
fn dma_oam_window_matches_oam_dma_timing() {
    for (offset, blocked) in [
        (1, false),
        (2, true),
        (100, true),
        (161, true),
        (162, false),
    ] {
        let mut b = new_bus();
        fill_wram(&mut b);
        b.write(DMA, 0xC0); // tick 0
        nops(&mut b, offset - 1);
        let v = b.read(0xFE00);
        if blocked {
            assert_eq!(v, 0xFF, "+{offset}");
        } else if offset == 1 {
            assert_eq!(v, 0x00, "+1: OAM still readable (old content)");
        } else {
            assert_eq!(v, b.wram[0], "+{offset}: transfer done");
        }
    }
}

#[test]
fn dma_copies_all_160_bytes_and_stops() {
    let mut b = new_bus();
    fill_wram(&mut b);
    b.write(DMA, 0xC1);
    nops(&mut b, 200);
    for i in 0..0xA0u16 {
        assert_eq!(b.read(0xFE00 + i), b.wram[0x100 + i as usize], "OAM[{i}]");
    }
    // Byte 160 of the source is not copied anywhere reachable: WRAM unchanged.
    assert_eq!(
        b.wram[0x1A0],
        (0x1A0u16 as u8).wrapping_mul(7).wrapping_add(1) | 1
    );
}

#[test]
fn dma_oam_writes_are_dropped_while_running() {
    let mut b = new_bus();
    fill_wram(&mut b);
    b.write(DMA, 0xC0);
    nops(&mut b, 4);
    b.write(0xFE05, 0xAA); // dropped
    nops(&mut b, 200);
    assert_eq!(b.read(0xFE05), b.wram[5]);
    // Not dropped afterwards.
    b.write(0xFE05, 0xAA);
    assert_eq!(b.read(0xFE05), 0xAA);
}

/// While the external bus is in use the CPU sees the byte being transferred.
#[test]
fn dma_from_wram_conflicts_with_rom_and_wram_not_hram_or_io() {
    let mut b = new_bus();
    fill_wram(&mut b);
    b.write(0xFF80, 0x99);
    b.write(DMA, 0xC0); // tick 0
    nops(&mut b, 2); // tick 1, 2
    // Copies run at ticks 2.., byte i at tick 2 + i; these reads are ticks 3, 4.
    assert_eq!(b.read(0x0000), b.wram[1], "ROM read returns DMA byte");
    assert_eq!(b.read(0xC123), b.wram[2], "WRAM read returns DMA byte");
    assert_eq!(b.read(0xFF80), 0x99, "HRAM always reachable");
    assert_eq!(b.read(DMA), 0xC0, "DMA register reachable");
    assert_eq!(b.read(0xFFFF), 0x00, "IE reachable");
    assert_eq!(b.read(IF) | 0xE0, b.interrupts.read_if() | 0xE0);
    // Writes to the conflicting bus are dropped; HRAM/IO writes land.
    b.write(0xC200, 0x12);
    b.write(0xFF81, 0x34);
    b.write(TMA, 0x56);
    nops(&mut b, 200);
    assert_ne!(b.wram[0x200], 0x12);
    assert_eq!(b.read(0xFF81), 0x34);
    assert_eq!(b.read(TMA), 0x56);
    assert_eq!(b.read(0x0000), 0x5A, "all normal again");
}

/// VRAM-sourced DMA conflicts with VRAM only; the external bus stays usable.
#[test]
fn dma_from_vram_conflicts_only_with_vram() {
    let mut b = new_bus();
    for i in 0..0xA0u16 {
        b.write(0x8000 + i, 0x80 | i as u8);
    }
    b.write(0xC000, 0x77);
    b.write(DMA, 0x80);
    nops(&mut b, 3);
    assert_eq!(b.read(0xC000), 0x77, "WRAM not in conflict");
    assert_eq!(b.read(0x0000), 0x5A, "ROM not in conflict");
    let v = b.read(0x8000);
    assert!(
        v & 0x80 != 0 && v != 0x5A,
        "VRAM read returns DMA byte: {v:#x}"
    );
    nops(&mut b, 200);
    assert_eq!(b.read(0xFE10), 0x80 | 0x10);
}

/// mooneye `oam_dma_restart`/`oam_dma_start`: restart extends the lock to
/// 161 cycles after the *second* write, with no gap.
#[test]
fn dma_restart_keeps_bus_locked_without_gap() {
    let mut b = new_bus();
    fill_wram(&mut b);
    b.write(DMA, 0xC0);
    nops(&mut b, 6);
    b.write(DMA, 0xC1); // restart at +7
    for i in 1..=161 {
        let v = b.read(0xFE00);
        assert_eq!(v, 0xFF, "+{i} after restart");
    }
    assert_eq!(b.read(0xFE00), b.wram[0x100], "+162: new source in OAM");
}

#[test]
fn dma_restart_start_comment_timeline() {
    // M=1 after a restart: previous DMA still locks OAM.
    let mut b = new_bus();
    fill_wram(&mut b);
    b.write(DMA, 0xC0);
    nops(&mut b, 20);
    b.write(DMA, 0xC0);
    assert_eq!(b.read(0xFE00), 0xFF, "M=1");
    // Fresh start: M=1 OAM accessible.
    let mut b = new_bus();
    b.write(DMA, 0xC0);
    assert_ne!(b.read(0xFE00), 0xFF, "fresh M=1");
}

/// mooneye `oam_dma/sources-GS`: sources >= 0xE0 read WRAM.
#[test]
fn dma_high_sources_alias_wram() {
    for (src, base) in [
        (0xE0u8, 0x0000usize),
        (0xE1, 0x0100),
        (0xFE, 0x1E00),
        (0xFF, 0x1F00),
    ] {
        let mut b = new_bus();
        fill_wram(&mut b);
        b.write(DMA, src);
        nops(&mut b, 170);
        assert_eq!(b.read(0xFE00), b.wram[base], "src {src:#x} first");
        assert_eq!(b.read(0xFE9F), b.wram[base + 0x9F], "src {src:#x} last");
    }
}

#[test]
fn dma_from_rom_and_cart() {
    let mut b = new_bus();
    b.write(DMA, 0x40);
    nops(&mut b, 170);
    assert_eq!(b.read(0xFE00), 0x6B);
    b.write(DMA, 0x00);
    nops(&mut b, 170);
    assert_eq!(b.read(0xFE00), 0x5A);
    // Cartridge RAM source with no RAM reads 0xFF.
    b.write(DMA, 0xA0);
    nops(&mut b, 170);
    assert_eq!(b.read(0xFE00), 0xFF);
}

#[test]
fn dma_register_reads_back_last_value() {
    let mut b = new_bus();
    assert_eq!(b.read(DMA), 0xFF);
    b.write(DMA, 0x12);
    assert_eq!(b.read(DMA), 0x12);
}

/// Total cost model: each read/write/tick advances exactly one M-cycle.
#[test]
fn every_access_costs_one_mcycle() {
    let mut b = new_bus();
    let start = b.mcycles();
    b.read(0);
    b.write(0xC000, 1);
    b.tick();
    assert_eq!(b.mcycles() - start, 3);
}
