//! KEY1 speed switch, double speed and HDMA/GDMA tests (DECISIONS #38, #40).
//! The ROM sets the CGB flag so the machine runs in CGB mode (`Hw::Cgb`).

use super::*;
use crate::bus::CpuBus;
use crate::cartridge;
use crate::model::Model;

const KEY1: u16 = 0xFF4D;
const HDMA1: u16 = 0xFF51;
const HDMA2: u16 = 0xFF52;
const HDMA3: u16 = 0xFF53;
const HDMA4: u16 = 0xFF54;
const HDMA5: u16 = 0xFF55;
const LY: u16 = 0xFF44;

fn machine(model: Model, flag: u8) -> Bus {
    let mut rom = vec![0u8; 0x8000];
    rom[0x143] = flag;
    // A recognisable pattern in ROM for GDMA sources.
    for (i, b) in rom[0x1000..0x1800].iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(7).wrapping_add(1);
    }
    let mut b = Bus::with_model(cartridge::from_rom(rom).unwrap(), model);
    b.interrupts.write_if(0);
    b
}

/// A CGB-mode bus with the LCD off.
fn cgb() -> Bus {
    let mut b = machine(Model::Cgb, 0x80);
    b.write(0xFF40, 0);
    b
}

/// A CGB-mode bus with the LCD on, at the start of line 0 of a frame.
fn cgb_lcd_on() -> Bus {
    let mut b = machine(Model::Cgb, 0x80);
    b.write(0xFF40, 0x91);
    while b.ppu.read_reg(LY) != 1 {
        b.tick();
    }
    while b.ppu.read_reg(LY) != 0 {
        b.tick();
    }
    b
}

/// STOP with a switch armed; returns the M-cycles it took.
fn switch(b: &mut Bus) -> u64 {
    b.write(KEY1, 1);
    let start = b.mcycles();
    assert!(b.stop());
    b.mcycles() - start
}

/// M-cycles between two frame-ready signals (LCD on).
fn mcycles_per_frame(b: &mut Bus) -> u64 {
    b.write(0xFF40, 0x91);
    let _ = b.ppu.take_frame_ready();
    while !b.ppu.take_frame_ready() {
        b.tick();
    }
    let start = b.mcycles();
    while !b.ppu.take_frame_ready() {
        b.tick();
    }
    b.mcycles() - start
}

fn vram(b: &Bus, off: usize) -> u8 {
    b.ppu.vram_bank(0)[off]
}

// ------------------------------------------------------------ speed switch --

#[test]
fn key1_switch_toggles_speed_and_clears_prepare() {
    let mut b = cgb();
    assert_eq!(b.read(KEY1), 0x7E);
    b.write(KEY1, 1);
    assert_eq!(b.read(KEY1), 0x7F);
    assert!(b.stop());
    assert_eq!(b.read(KEY1), 0xFE, "double speed, prepare cleared");
    assert!(b.double_speed());
    // Armed again: back to normal speed.
    b.write(KEY1, 1);
    assert_eq!(b.read(KEY1), 0xFF);
    assert!(b.stop());
    assert_eq!(b.read(KEY1), 0x7E);
    assert!(!b.double_speed());
}

#[test]
fn stop_without_prepare_does_not_switch() {
    let mut b = cgb();
    assert!(!b.stop());
    assert_eq!(b.read(KEY1), 0x7E);
    assert!(!b.slow);
}

#[test]
fn stop_with_a_selected_joypad_line_low_does_not_switch() {
    use crate::joypad::Button;
    let mut b = cgb();
    b.write(0xFF00, 0x20);
    b.joypad.set_button(Button::Down, true, &mut b.interrupts);
    b.write(KEY1, 1);
    assert!(!b.stop());
    assert_eq!(b.read(KEY1), 0x7F, "still armed");
}

#[test]
fn compat_mode_and_dmg_never_switch() {
    for model in [Model::Dmg, Model::Cgb] {
        let mut b = machine(model, 0x00);
        b.write(KEY1, 1);
        assert_eq!(b.read(KEY1), 0xFF, "unmapped");
        assert!(!b.stop());
        assert!(!b.slow && !b.double_speed());
    }
}

#[test]
fn speed_switch_pauses_for_32768_mcycles_with_the_timer_running() {
    let mut b = cgb();
    for _ in 0..300 {
        b.tick();
    }
    // A 4 kHz TIMA (256 M-cycles per step) counts 128 steps in the pause.
    b.write(0xFF07, 0x04);
    b.write(0xFF05, 0);
    b.write(KEY1, 1);
    let start = b.mcycles();
    assert!(b.stop());
    // The padding byte's cycle, then the pause.
    assert_eq!(b.mcycles() - start, 1 + u64::from(SPEED_SWITCH_MCYCLES));
    assert_eq!(b.timer.div_counter(), 0, "2^17 counter steps: DIV reads 0");
    let tima = b.read(0xFF05);
    assert!((0x80..=0x81).contains(&tima), "TIMA = 128 steps: {tima:#X}");
    b.tick();
    assert_eq!(
        b.timer.div_counter(),
        8,
        "and it counts on (the read above ticked once)"
    );
}

#[test]
fn a_pending_interrupt_ends_the_pause() {
    let mut b = cgb();
    b.write(0xFFFF, 0x04);
    b.interrupts.write_if(0x04);
    assert_eq!(switch(&mut b), 1, "only the padding cycle");
    assert!(b.double_speed());
    // One that arrives mid-pause ends it too.
    b.interrupts.write_if(0);
    b.write(0xFF07, 0x05); // 262 kHz: TIMA overflows after 1024 M-cycles
    b.write(0xFF05, 0);
    let took = switch(&mut b);
    assert!((1000..=1100).contains(&took), "{took}");
}

#[test]
fn double_speed_frame_takes_twice_the_mcycles() {
    let mut b = cgb();
    let normal = mcycles_per_frame(&mut b);
    assert_eq!(normal, 17_556);
    switch(&mut b);
    assert_eq!(mcycles_per_frame(&mut b), 2 * normal);
    // Back to normal speed.
    switch(&mut b);
    assert_eq!(mcycles_per_frame(&mut b), normal);
}

#[test]
fn lcd_off_frame_cadence_is_real_time_in_double_speed() {
    let mut b = cgb();
    switch(&mut b);
    while !b.ppu.take_frame_ready() {
        b.tick();
    }
    let start = b.mcycles();
    while !b.ppu.take_frame_ready() {
        b.tick();
    }
    assert_eq!(b.mcycles() - start, 2 * 17_556);
}

#[test]
fn timer_runs_twice_as_fast_relative_to_the_frame() {
    // TAC = 0b100: 4096 Hz, one TIMA step per 256 CPU M-cycles.
    let tima_per_frame = |double: bool| {
        let mut b = cgb();
        if double {
            switch(&mut b);
        }
        b.write(0xFF40, 0x91);
        b.write(0xFF07, 0x04);
        b.write(0xFF05, 0);
        let _ = b.ppu.take_frame_ready();
        while !b.ppu.take_frame_ready() {
            b.tick();
        }
        b.write(0xFF05, 0);
        while !b.ppu.take_frame_ready() {
            b.tick();
        }
        u32::from(b.read(0xFF05))
    };
    let (n, d) = (tima_per_frame(false), tima_per_frame(true));
    assert!((68..=69).contains(&n), "{n}");
    assert!(d.abs_diff(2 * n) <= 1, "normal {n}, double {d}");
}

#[test]
fn apu_sample_count_per_real_second_is_unchanged_in_double_speed() {
    let samples_per_frame = |double: bool| {
        let mut b = cgb();
        if double {
            switch(&mut b);
        }
        b.write(0xFF40, 0x91);
        let _ = b.ppu.take_frame_ready();
        let mut out = Vec::new();
        b.apu.drain_samples(&mut out);
        out.clear();
        let mut n = 0;
        for _ in 0..60 {
            while !b.ppu.take_frame_ready() {
                b.tick();
            }
            b.apu.drain_samples(&mut out);
            n = out.len();
        }
        n
    };
    let (n, d) = (samples_per_frame(false), samples_per_frame(true));
    // 60 frames of 48 kHz stereo is about 1 s: 2 * 48000 * 60 / 59.73.
    assert!(n.abs_diff(d) <= 4, "normal {n}, double {d}");
    assert!(n.abs_diff(2 * 48_000) < 2 * 1_000);
}

#[test]
fn apu_frame_sequencer_keeps_its_real_time_rate() {
    // Length counter of CH2 (NR21 length 62 -> 2 length ticks), stepped at
    // 256 Hz: it expires after the same number of real M-cycles at either speed.
    let expiry = |double: bool| {
        let mut b = cgb();
        b.write(0xFF26, 0x80);
        if double {
            switch(&mut b);
        }
        b.write(0xFF17, 0xF0); // DAC on
        b.write(0xFF16, 62); // length = 64 - 62 = 2
        b.write(0xFF19, 0xC0); // trigger, length enabled
        let start = b.mcycles();
        while b.read(0xFF26) & 2 != 0 {
            b.tick();
        }
        (b.mcycles() - start) as f64 * if double { 0.5 } else { 1.0 }
    };
    let (n, d) = (expiry(false), expiry(true));
    assert!(
        (n - d).abs() <= 2.0 * 1024.0 / 4.0,
        "normal {n}, double {d}"
    );
    assert!(n > 4000.0, "{n}");
}

#[test]
fn oam_dma_runs_per_cpu_mcycle_in_double_speed() {
    let mut b = cgb();
    switch(&mut b);
    b.write(0xFF46, 0xC0);
    let start = b.mcycles();
    while b.dma.is_blocking() || b.mcycles() - start < 3 {
        b.tick();
    }
    let took = b.mcycles() - start;
    assert!((161..=163).contains(&took), "{took}");
}

#[test]
fn halt_ticks_do_not_change_the_clocks() {
    let mut a = cgb();
    let mut b = cgb();
    switch(&mut a);
    switch(&mut b);
    for _ in 0..1000 {
        a.tick();
        b.idle_tick();
    }
    assert_eq!(a.timer.div_counter(), b.timer.div_counter());
    assert_eq!(a.mcycles(), b.mcycles());
}

// -------------------------------------------------------------------- GDMA --

fn setup_dma(b: &mut Bus, src: u16, dst: u16) {
    b.write(HDMA1, (src >> 8) as u8);
    b.write(HDMA2, src as u8);
    b.write(HDMA3, (dst >> 8) as u8);
    b.write(HDMA4, dst as u8);
}

#[test]
fn gdma_copies_the_length_and_stalls_the_cpu() {
    let mut b = cgb();
    setup_dma(&mut b, 0x1000, 0x8000);
    let start = b.mcycles();
    b.write(HDMA5, 0x01); // 2 blocks
    // The write's own cycle, one start-up cycle, 8 per block.
    assert_eq!(b.mcycles() - start, 1 + 1 + 16);
    for i in 0..32 {
        assert_eq!(vram(&b, i), b.cart.read_rom(0x1000 + i as u16), "byte {i}");
    }
    assert_eq!(vram(&b, 32), 0);
    assert_eq!(b.read(HDMA5), 0xFF, "complete");
    assert!(!b.slow);
}

#[test]
fn gdma_stall_is_the_same_real_time_in_double_speed() {
    let mut b = cgb();
    switch(&mut b);
    setup_dma(&mut b, 0x1000, 0x8000);
    let start = b.mcycles();
    b.write(HDMA5, 0x03); // 4 blocks
    assert_eq!(b.mcycles() - start, 1 + 1 + 64);
    assert_eq!(vram(&b, 63), b.cart.read_rom(0x1000 + 63));
}

#[test]
fn gdma_ticks_the_other_components_during_the_stall() {
    let mut b = cgb_lcd_on();
    setup_dma(&mut b, 0x1000, 0x8000);
    let before = b.timer.div_counter();
    b.write(HDMA5, 0x7F); // 128 blocks = 1024 M-cycles + 2
    assert_eq!(b.timer.div_counter().wrapping_sub(before), 4 * 1026);
    assert_ne!(b.ppu.read_reg(LY), 0, "the PPU ran on");
}

#[test]
fn gdma_ignores_the_ppu_lock_and_uses_the_vbk_bank() {
    let mut b = cgb_lcd_on();
    while b.ppu.read_reg(0xFF41) & 3 != 3 {
        b.tick();
    }
    b.write(0xFF4F, 1);
    setup_dma(&mut b, 0x1000, 0x8010);
    b.write(HDMA5, 0x00);
    assert_eq!(b.ppu.vram_bank(1)[0x10], b.cart.read_rom(0x1000));
    assert_eq!(b.ppu.vram_bank(0)[0x10], 0);
}

#[test]
fn address_registers_are_masked_and_write_only() {
    let mut b = cgb();
    b.write(0xC000, 0xAA);
    // Source 0xC00F -> 0xC000, destination 0x3FF5 -> 0x9FF0 & 0x1FF0 = 0x1FF0.
    b.write(HDMA1, 0xC0);
    b.write(HDMA2, 0x0F);
    b.write(HDMA3, 0x3F);
    b.write(HDMA4, 0xF5);
    for a in [HDMA1, HDMA2, HDMA3, HDMA4] {
        assert_eq!(b.read(a), 0xFF);
    }
    b.write(HDMA5, 0x00);
    assert_eq!(vram(&b, 0x1FF0), 0xAA);
}

#[test]
fn vram_and_high_sources_read_open_bus() {
    for src in [0x8000u16, 0x9FF0, 0xE000, 0xFF00] {
        let mut b = cgb();
        b.write(0x8000, 0x11);
        setup_dma(&mut b, src, 0x8100);
        b.write(HDMA5, 0x00);
        assert_eq!(vram(&b, 0x100), 0xFF, "source {src:04X}");
    }
}

#[test]
fn the_destination_wrap_stops_the_transfer_early() {
    let mut b = cgb();
    setup_dma(&mut b, 0x1000, 0x9FF0);
    b.write(HDMA5, 0x03); // 4 blocks, but only one fits
    assert_eq!(b.read(HDMA5), 0x80 | 2, "3 blocks stay");
    assert_eq!(vram(&b, 0x1FF0), b.cart.read_rom(0x1000));
    assert_eq!(vram(&b, 0), 0, "nothing wrapped to the start of VRAM");
}

// -------------------------------------------------------------------- HDMA --

/// Run busy ticks until `ly` reaches `line`'s start.
fn run_to_line(b: &mut Bus, line: u8) {
    while b.ppu.read_reg(LY) != line {
        b.tick();
    }
}

fn filled_wram(b: &mut Bus) {
    for i in 0..0x100u16 {
        b.write(0xC000 + i, (i as u8) ^ 0x5C);
    }
}

#[test]
fn hdma_copies_one_block_per_hblank_and_reads_back_the_remaining_length() {
    let mut b = cgb_lcd_on();
    filled_wram(&mut b);
    setup_dma(&mut b, 0xC000, 0x8000);
    run_to_line(&mut b, 20);
    b.write(HDMA5, 0x83); // 4 blocks, HBlank mode
    assert!(b.slow);
    assert_eq!(b.hdma.read5(), 0x03, "bit 7 clear while active");
    // Sampled at the start of each line, FF55 drops by one per line (the
    // peek goes around the busy tick that would start a block).
    let mut seen = Vec::new();
    for line in 21..=26u8 {
        run_to_line(&mut b, line);
        seen.push(b.hdma.read5());
    }
    assert_eq!(seen, [0x02, 0x01, 0x00, 0xFF, 0xFF, 0xFF]);
    assert!(!b.slow, "finished");
    for k in 0..4u16 {
        let off = usize::from(k) * 16;
        let want: Vec<u8> = (0..16).map(|i| ((off + i) as u8) ^ 0x5C).collect();
        assert_eq!(vram_range(&b, off), &want[..], "block {k}");
    }
    assert_eq!(vram(&b, 64), 0);
}

fn vram_range(b: &Bus, off: usize) -> &[u8] {
    &b.ppu.vram_bank(0)[off..off + 16]
}

#[test]
fn hdma_stalls_the_cpu_for_one_burst_per_block() {
    // 500 busy ticks through a line with a 1-block HDMA armed take 500 plus the
    // burst's 1 + 8 M-cycles of the clock.
    let mut b = cgb_lcd_on();
    run_to_line(&mut b, 4);
    setup_dma(&mut b, 0xC000, 0x8000);
    b.write(HDMA5, 0x80);
    let start = b.mcycles();
    for _ in 0..500 {
        b.tick();
    }
    assert_eq!(b.hdma.read5(), 0xFF, "the block ran");
    assert_eq!(b.mcycles() - start, 500 + 1 + 8);
}

#[test]
fn hdma_does_not_run_in_vblank() {
    let mut b = cgb_lcd_on();
    setup_dma(&mut b, 0xC000, 0x8000);
    run_to_line(&mut b, 143);
    run_to_line(&mut b, 144); // the line-143 block is long done
    b.write(HDMA5, 0x8F); // 16 blocks, started in VBlank
    let left = b.read(HDMA5);
    for _ in 0..10 * 114 {
        b.tick();
    }
    assert_eq!(b.read(HDMA5), left, "no block through VBlank");
    run_to_line(&mut b, 1);
    run_to_line(&mut b, 2);
    assert_eq!(b.read(HDMA5), left - 2, "resumes at line 0/1");
}

#[test]
fn hdma_runs_one_block_per_hblank_in_double_speed_too() {
    let mut b = cgb();
    switch(&mut b);
    let mut b = {
        b.write(0xFF40, 0x91);
        run_to_line(&mut b, 1);
        b
    };
    setup_dma(&mut b, 0xC000, 0x8000);
    b.write(HDMA5, 0x87);
    let mut lines = 0;
    let mut prev = b.read(HDMA5);
    for line in 2..=4 {
        run_to_line(&mut b, line);
        let now = b.read(HDMA5);
        assert!(
            now < prev || now == 0xFF,
            "line {line}: {prev:02X} -> {now:02X}"
        );
        prev = now;
        lines += 1;
    }
    assert_eq!(lines, 3);
}

#[test]
fn writing_bit7_clear_cancels_and_loads_the_written_length() {
    let mut b = cgb_lcd_on();
    setup_dma(&mut b, 0xC000, 0x8000);
    run_to_line(&mut b, 10);
    b.write(HDMA5, 0x85); // 6 blocks
    run_to_line(&mut b, 13);
    let remaining = b.hdma.read5();
    assert!(remaining < 0x05 && remaining & 0x80 == 0);
    b.write(HDMA5, 0x03);
    assert!(!b.hdma.hblank_active() && !b.slow);
    // SameSuite hdma_lcd_off / hdma_mode0: cancelling with 0 reads back $80.
    assert_eq!(b.read(HDMA5), 0x80 | 0x03);
    let copied = vram(&b, 0x20);
    run_to_line(&mut b, 20);
    assert_eq!(b.read(HDMA5), 0x83, "nothing more copied");
    assert_eq!(vram(&b, 0x20), copied);
    b.write(HDMA5, 0x80);
    b.write(HDMA5, 0x00);
    assert_eq!(b.read(HDMA5), 0x80);
}

#[test]
fn a_halted_cpu_does_not_run_hdma_but_the_block_follows_on_waking_in_hblank() {
    let mut b = cgb_lcd_on();
    setup_dma(&mut b, 0xC000, 0x8000);
    run_to_line(&mut b, 10);
    b.write(HDMA5, 0x87);
    run_to_line(&mut b, 12);
    let left = b.hdma.read5();
    // Idle through line 12 and well into its HBlank.
    while !b.ppu.in_hdma_window() {
        b.idle_tick();
    }
    for _ in 0..10 {
        b.idle_tick();
    }
    assert!(b.ppu.in_hdma_window());
    assert_eq!(b.hdma.read5(), left, "no block while halted");
    b.tick();
    assert_eq!(b.hdma.read5(), left - 1, "block on the first busy cycle");
    // Idling across the whole of the next HBlank and into the following line
    // drops that line's block.
    let left = b.hdma.read5();
    while b.ppu.in_hdma_window() {
        b.idle_tick();
    }
    while !b.ppu.in_hdma_window() {
        b.idle_tick();
    }
    while b.ppu.in_hdma_window() {
        b.idle_tick();
    }
    b.tick();
    assert_eq!(b.hdma.read5(), left, "the missed HBlank's block is dropped");
}

#[test]
fn the_speed_switch_pause_runs_no_hdma_block_and_drops_a_stale_signal() {
    let (mut inside, mut outside) = (0, 0);
    // Start the STOP at every few M-cycles of a line, so the pause (about 144
    // lines) ends all over the line, in and out of HBlank.
    for k in (0..114).step_by(3) {
        let mut b = cgb_lcd_on();
        setup_dma(&mut b, 0xC000, 0x8000);
        run_to_line(&mut b, 20);
        b.write(HDMA5, 0x8F); // 16 blocks, more than the pause could drain
        for _ in 0..k {
            b.idle_tick();
        }
        let before = b.hdma.read5();
        b.write(KEY1, 1);
        assert!(b.stop());
        let after = b.hdma.read5();
        // At most the padding cycle's busy tick (before the pause) ran a block.
        assert!(before - after <= 1, "k={k}: blocks ran in the pause");
        let in_hblank = b.ppu.in_hdma_window();
        let signalled = b.ppu.take_hblank_entered();
        assert!(
            !signalled || in_hblank,
            "k={k}: a signal from an earlier line survived the pause"
        );
        b.ppu.set_hblank_entered(signalled);
        b.tick();
        let fired = after - b.hdma.read5();
        assert_eq!(fired, u8::from(in_hblank), "k={k}");
        if in_hblank {
            inside += 1;
        } else {
            outside += 1;
        }
    }
    assert!(inside > 0 && outside > 0, "{inside} in, {outside} out");
}

#[test]
fn lcd_off_runs_one_block_on_start_and_then_none() {
    let mut b = cgb();
    setup_dma(&mut b, 0xC000, 0x8000);
    b.write(0xC000, 0x42);
    b.write(HDMA5, 0x83);
    b.tick();
    assert_eq!(b.read(HDMA5), 0x02, "one block ran at once");
    for _ in 0..2000 {
        b.tick();
    }
    assert_eq!(b.read(HDMA5), 0x02, "no more blocks with the LCD off");
    assert_eq!(vram(&b, 0), 0x42);
}

#[test]
fn switching_the_lcd_off_outside_hblank_gives_the_armed_hdma_one_block() {
    let mut b = cgb_lcd_on();
    setup_dma(&mut b, 0xC000, 0x8000);
    b.write(HDMA5, 0x87);
    run_to_line(&mut b, 2);
    while b.ppu.in_hdma_window() {
        b.tick();
    }
    let left = b.read(HDMA5);
    b.write(0xFF40, 0x11); // LCD off in mode 2/3
    b.tick();
    assert_eq!(b.read(HDMA5), left - 1);
}

#[test]
fn hdma_registers_are_unmapped_off_cgb_mode() {
    for (model, flag) in [(Model::Dmg, 0x80), (Model::Cgb, 0x00)] {
        let mut b = machine(model, flag);
        for a in HDMA1..=HDMA5 {
            b.write(a, 0x00);
            assert_eq!(b.read(a), 0xFF, "{a:04X} {model:?} {flag:02X}");
        }
        assert!(!b.slow);
    }
}

#[test]
fn state_round_trips_mid_hdma_and_in_double_speed() {
    use crate::state::{StateReader, StateWriter};
    let mut b = cgb_lcd_on();
    switch(&mut b);
    setup_dma(&mut b, 0xC000, 0x8000);
    run_to_line(&mut b, 3);
    b.write(HDMA5, 0x8F);
    run_to_line(&mut b, 6);
    let mut w = StateWriter::new();
    b.save_state(&mut w);
    let bytes = w.into_bytes();
    let mut c = machine(Model::Cgb, 0x80);
    c.load_state(&mut StateReader::new(&bytes)).unwrap();
    assert!(c.double_speed() && c.slow && c.hdma.hblank_active());
    assert_eq!(c.read(HDMA5), b.read(HDMA5));
    for _ in 0..5000 {
        b.tick();
        c.tick();
    }
    assert_eq!(b.read(HDMA5), c.read(HDMA5));
    assert_eq!(b.mcycles(), c.mcycles());
    assert_eq!(b.ppu.vram_bank(0)[..0x100], c.ppu.vram_bank(0)[..0x100]);
}
