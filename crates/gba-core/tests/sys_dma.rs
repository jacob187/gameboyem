//! DMA and timers, driven through the bus.

mod sys_common;
use gba_core::bus::CpuBus;
use gba_core::dma::DmaTrigger;
use gba_core::ppu::HDRAW_CYCLES;
use sys_common::*;

const IF: u32 = 0x0400_0202;
const IWRAM: u32 = 0x0300_0000;
const EWRAM: u32 = 0x0200_0000;

fn fill16(b: &mut gba_core::bus::Bus, addr: u32, n: u32) {
    for i in 0..n {
        b.write16(addr + 2 * i, 0x1000 + i as u16, N);
    }
}

fn read_all16(b: &mut gba_core::bus::Bus, addr: u32, n: u32) -> Vec<u16> {
    (0..n).map(|i| b.read16(addr + 2 * i, N)).collect()
}

#[test]
fn immediate_copy_and_its_cycle_cost() {
    let mut b = bus();
    fill16(&mut b, EWRAM, 4);
    start_dma(&mut b, 3, EWRAM, IWRAM, 4, 0x8000);
    // 2 start cycles, then 4 units of (EWRAM read 3 + IWRAM write 1).
    assert_eq!(cycles(&mut b, settle), 2 + 4 * 4);
    assert_eq!(
        read_all16(&mut b, IWRAM, 4),
        [0x1000, 0x1001, 0x1002, 0x1003]
    );
    assert_eq!(b.read16(0x0400_00DE, N), 0, "enable clears when done");
    assert!(b.dma.active_channel().is_none());
}

#[test]
fn word_units_and_n_then_s_costs() {
    let mut b = bus();
    b.write32(IWRAM, 0xAABB_CCDD, N);
    b.write32(IWRAM + 4, 0x1122_3344, N);
    b.write32(IWRAM + 8, 0x5566_7788, N);
    start_dma(&mut b, 3, IWRAM, IWRAM + 0x100, 3, 0x8000 | 1 << 10);
    assert_eq!(cycles(&mut b, settle), 2 + 3 * 2);
    assert_eq!(b.read32(IWRAM + 0x104, N), 0x1122_3344);
    // ROM source: first unit N (5), second S (3); IWRAM writes 1 each.
    let mut rom = rom_with(&[]);
    rom[0x10..0x14].copy_from_slice(&[1, 2, 3, 4]);
    let mut b = bus_with(rom);
    start_dma(&mut b, 3, 0x0800_0010, IWRAM, 2, 0x8000);
    assert_eq!(cycles(&mut b, settle), 2 + (5 + 1) + (3 + 1));
    assert_eq!(b.read16(IWRAM, N), 0x0201);
    // Cartridge to cartridge costs 4 start cycles.
    let mut b = bus();
    start_dma(&mut b, 3, 0x0800_0000, 0x0800_0100, 1, 0x8000);
    assert_eq!(cycles(&mut b, settle), 4 + 5 + 5);
}

#[test]
fn destination_decrement() {
    let mut b = bus();
    fill16(&mut b, IWRAM, 4);
    start_dma(&mut b, 3, IWRAM, IWRAM + 0x106, 4, 0x8000 | 1 << 5);
    settle(&mut b);
    assert_eq!(
        read_all16(&mut b, IWRAM + 0x100, 4),
        [0x1003, 0x1002, 0x1001, 0x1000]
    );
}

#[test]
fn fixed_source_and_decrementing_source() {
    let mut b = bus();
    fill16(&mut b, IWRAM, 4);
    start_dma(&mut b, 3, IWRAM + 2, IWRAM + 0x100, 3, 0x8000 | 2 << 7);
    settle(&mut b);
    assert_eq!(read_all16(&mut b, IWRAM + 0x100, 3), [0x1001; 3]);
    start_dma(&mut b, 3, IWRAM + 6, IWRAM + 0x200, 4, 0x8000 | 1 << 7);
    settle(&mut b);
    assert_eq!(
        read_all16(&mut b, IWRAM + 0x200, 4),
        [0x1003, 0x1002, 0x1001, 0x1000]
    );
}

#[test]
fn unaligned_addresses_are_aligned_down() {
    let mut b = bus();
    b.write32(IWRAM, 0x1122_3344, N);
    start_dma(&mut b, 3, IWRAM + 3, IWRAM + 0x103, 1, 0x8000 | 1 << 10);
    settle(&mut b);
    assert_eq!(b.read32(IWRAM + 0x100, N), 0x1122_3344);
}

#[test]
fn address_and_count_masks() {
    let mut b = bus();
    fill16(&mut b, EWRAM, 2);
    // DMA0 has a 27-bit source: 0x0A00_0000 reads EWRAM.
    start_dma(&mut b, 0, 0x0A00_0000, IWRAM, 2, 0x8000);
    settle(&mut b);
    assert_eq!(read_all16(&mut b, IWRAM, 2), [0x1000, 0x1001]);
    // DMA0 count 0 means 0x4000 units; copy 32 KiB over 32 KiB of EWRAM.
    start_dma(&mut b, 0, IWRAM, EWRAM + 0x8000, 0, 0x8000 | 1 << 10);
    let n = cycles(&mut b, settle);
    assert_eq!(
        n,
        2 + 0x4000 * (1 + 6),
        "0x4000 words, IWRAM read 1 + EWRAM word write 6"
    );
    assert_eq!(b.read16(EWRAM + 0x8000, N), 0x1000);
    // The control register keeps only its defined bits; bit 11 only on DMA3.
    b.write16(0x0400_00BA, 0x7FFF, N);
    assert_eq!(b.read16(0x0400_00BA, N), 0x77E0);
    b.write16(0x0400_00DE, 0x7FFF, N);
    assert_eq!(b.read16(0x0400_00DE, N), 0x7FE0);
}

#[test]
fn irq_only_when_requested() {
    let mut b = bus();
    start_dma(&mut b, 3, IWRAM, IWRAM + 0x10, 1, 0x8000);
    settle(&mut b);
    assert_eq!(b.read16(IF, N) & 1 << 11, 0);
    start_dma(&mut b, 3, IWRAM, IWRAM + 0x10, 1, 0x8000 | 1 << 14);
    settle(&mut b);
    assert_eq!(b.read16(IF, N) & 1 << 11, 1 << 11);
    start_dma(&mut b, 1, IWRAM, IWRAM + 0x10, 1, 0x8000 | 1 << 14);
    settle(&mut b);
    assert_eq!(b.read16(IF, N) & 1 << 9, 1 << 9);
}

#[test]
fn lower_numbered_channels_run_first() {
    let mut b = bus();
    b.write16(IWRAM + 0x400, 0xAAAA, N);
    b.write16(IWRAM + 0x500, 0xBBBB, N);
    // Both copy to the same place; DMA3 is programmed first but runs last.
    start_dma(&mut b, 3, IWRAM + 0x400, IWRAM + 0x100, 1, 0x8000);
    start_dma(&mut b, 1, IWRAM + 0x500, IWRAM + 0x100, 1, 0x8000);
    assert_eq!(b.dma.active_channel(), Some(1));
    settle(&mut b);
    assert_eq!(b.read16(IWRAM + 0x100, N), 0xAAAA);
}

#[test]
fn writing_enable_off_cancels() {
    let mut b = bus();
    fill16(&mut b, IWRAM, 4);
    start_dma(&mut b, 3, IWRAM, IWRAM + 0x100, 4, 0x8000);
    b.write16(0x0400_00DE, 0, N);
    assert!(b.dma.active_channel().is_none());
    assert_eq!(b.read16(IWRAM + 0x100, N), 0);
}

#[test]
fn reading_bios_by_dma_returns_the_latch() {
    let mut b = bus();
    b.write32(IWRAM, 0xCAFE_F00D, N);
    start_dma(&mut b, 3, IWRAM, IWRAM + 0x10, 1, 0x8000 | 1 << 10);
    settle(&mut b);
    start_dma(&mut b, 3, 0x0000_0000, IWRAM + 0x20, 1, 0x8000 | 1 << 10);
    settle(&mut b);
    assert_eq!(b.read32(IWRAM + 0x20, N), 0xCAFE_F00D);
}

#[test]
fn hblank_dma_waits_for_the_trigger_and_repeats() {
    let mut b = bus();
    fill16(&mut b, IWRAM, 4);
    // HBlank, repeat, destination increment+reload, 2 halfwords.
    start_dma(
        &mut b,
        0,
        IWRAM,
        IWRAM + 0x100,
        2,
        0x8000 | 2 << 12 | 1 << 9 | 3 << 5,
    );
    assert!(b.dma.active_channel().is_none());
    // The PPU's HBlank signal (line 0, cycle 1006) starts it.
    run_to(&mut b, u64::from(HDRAW_CYCLES));
    assert_eq!(b.dma.active_channel(), Some(0));
    settle(&mut b);
    assert_eq!(read_all16(&mut b, IWRAM + 0x100, 2), [0x1000, 0x1001]);
    assert_ne!(
        b.read16(0x0400_00BA, N) & 0x8000,
        0,
        "repeat keeps it enabled"
    );
    // Next line: the source continues, the destination restarts at its reload.
    b.write16(IWRAM + 0x100, 0xFFFF, N);
    run_to(&mut b, 1232 + u64::from(HDRAW_CYCLES));
    settle(&mut b);
    assert_eq!(b.read16(IWRAM + 0x100, N), 0x1002);
    assert_eq!(b.read16(IWRAM + 0x102, N), 0x1003);
    assert_eq!(b.read16(IWRAM + 0x104, N), 0, "not past the reload");
}

#[test]
fn vblank_dma_starts_at_line_160() {
    let mut b = bus();
    start_dma(
        &mut b,
        3,
        IWRAM,
        IWRAM + 0x100,
        1,
        0x8000 | 1 << 12 | 1 << 14,
    );
    run_to(&mut b, 160 * 1232 - 1);
    assert!(b.dma.active_channel().is_none());
    run_to(&mut b, 160 * 1232);
    assert_eq!(b.dma.active_channel(), Some(3));
    settle(&mut b);
    assert_eq!(b.read16(IF, N) & 1 << 11, 1 << 11);
    assert_eq!(b.read16(0x0400_00DE, N) & 0x8000, 0, "no repeat: done");
}

#[test]
fn video_capture_runs_lines_2_to_161() {
    let mut b = bus();
    // DMA3 special timing, repeat.
    start_dma(
        &mut b,
        3,
        IWRAM,
        IWRAM + 0x100,
        1,
        0x8000 | 3 << 12 | 1 << 9,
    );
    run_to(&mut b, 2 * 1232 - 1);
    assert!(b.dma.active_channel().is_none());
    run_to(&mut b, 2 * 1232);
    assert_eq!(b.dma.active_channel(), Some(3));
    settle(&mut b);
    run_to(&mut b, 100 * 1232);
    settle(&mut b);
    assert_ne!(b.read16(0x0400_00DE, N) & 0x8000, 0);
    run_to(&mut b, 162 * 1232);
    settle(&mut b);
    assert_eq!(b.read16(0x0400_00DE, N) & 0x8000, 0, "stopped at line 162");
}

#[test]
fn fifo_dma_moves_four_words_when_requested() {
    let mut b = bus();
    for i in 0..4 {
        b.write32(IWRAM + 4 * i, i, N);
    }
    // DMA1: special timing, repeat, fixed destination = FIFO A; count ignored.
    start_dma(
        &mut b,
        1,
        IWRAM,
        0x0400_00A0,
        1,
        0x8000 | 3 << 12 | 1 << 9 | 2 << 5,
    );
    b.dma.trigger(DmaTrigger::FifoB);
    assert!(b.dma.active_channel().is_none(), "wrong FIFO");
    b.dma.trigger(DmaTrigger::VBlank);
    assert!(b.dma.active_channel().is_none());
    b.dma.trigger(DmaTrigger::FifoA);
    assert_eq!(b.dma.active_channel(), Some(1));
    // 4 words: IWRAM read 1 + I/O write 1 each.
    assert_eq!(cycles(&mut b, settle), 2 + 4 * 2);
    assert_ne!(b.read16(0x0400_00C6, N) & 0x8000, 0, "stays enabled");
    // And again on the next request, from where it left off.
    b.dma.trigger(DmaTrigger::FifoA);
    assert_eq!(cycles(&mut b, settle), 2 + 4 * 2);
}

#[test]
fn timer_counts_and_overflows_with_an_irq() {
    let mut b = bus();
    // 32-bit write: reload 0xFF00 + enable and IRQ in one go.
    b.write32(0x0400_0100, 0xFF00 | 0xC0 << 16, N);
    assert_eq!(b.read16(0x0400_0100, N), 0xFF00, "start delay");
    run(&mut b, 10);
    let v = b.read16(0x0400_0100, N);
    assert!((0xFF00..0xFF20).contains(&v), "{v:#x}");
    assert_eq!(b.read16(IF, N) & 1 << 3, 0);
    run(&mut b, 300);
    assert_eq!(b.read16(IF, N) & 1 << 3, 1 << 3);
    let v = b.read16(0x0400_0100, N);
    assert!((0xFF00..0xFF80).contains(&v), "reloaded: {v:#x}");
    // Control reads back only its defined bits.
    assert_eq!(b.read16(0x0400_0102, N), 0xC0);
}

#[test]
fn timer_counter_is_live_between_events() {
    let mut b = bus();
    b.write32(0x0400_0104, 0x80 << 16 | 1 << 16, N); // reload 0, F/64, enable
    let t0 = b.sched.now();
    b.idle(64 * 5 + 2);
    assert_eq!(
        b.read16(0x0400_0104, N),
        5,
        "at {} cycles",
        b.sched.now() - t0
    );
}

#[test]
fn cascade_timer_counts_overflows() {
    let mut b = bus();
    b.write32(0x0400_0104, 0xFFFE | 0xC4 << 16, N); // timer 1: count-up + IRQ
    b.write32(0x0400_0100, 0xFFFF | 0x80 << 16, N); // timer 0 overflows every tick
    run(&mut b, 3);
    assert_eq!(b.read16(IF, N) & 1 << 4, 0, "needs two overflows");
    assert_eq!(b.read16(0x0400_0104, N), 0xFFFF);
    run(&mut b, 3);
    assert_eq!(b.read16(IF, N) & 1 << 4, 1 << 4);
}

#[test]
fn stopped_timer_freezes() {
    let mut b = bus();
    b.write32(0x0400_0108, 0x80 << 16, N);
    b.idle(100);
    b.write16(0x0400_010A, 0, N);
    let v = b.read16(0x0400_0108, N);
    assert!(v > 90 && v <= 100, "{v}");
    b.idle(1000);
    assert_eq!(b.read16(0x0400_0108, N), v);
}

#[test]
fn eeprom_roundtrip_over_dma3() {
    let mut b = bus_with(rom_with(b"EEPROM_V124"));
    let bits = |v: &[u8]| v.iter().map(|&x| u16::from(x)).collect::<Vec<_>>();
    // Write request for a 512 B chip: 2 + 6 + 64 + 1 = 73 halfwords.
    let mut req = vec![1u8, 0];
    req.extend((0..6).rev().map(|i| (5u8 >> i) & 1));
    let value = 0x0123_4567_89AB_CDEFu64;
    req.extend((0..64).rev().map(|i| ((value >> i) & 1) as u8));
    req.push(0);
    assert_eq!(req.len(), 73);
    for (i, h) in bits(&req).into_iter().enumerate() {
        b.write16(EWRAM + 2 * i as u32, h, N);
    }
    assert!(b.cart.battery_ram().is_none(), "size unknown yet");
    start_dma(&mut b, 3, EWRAM, 0x0D00_0000, 73, 0x8000);
    settle(&mut b);
    assert_eq!(b.cart.battery_ram().unwrap().len(), 512);
    assert_eq!(b.cart.battery_generation(), 1);
    assert_eq!(b.read16(0x0D00_0000, N), 1, "ready");
    // Read request (2 + 6 + 1 = 9 halfwords), then a 68-halfword read.
    let mut req = vec![1u8, 1];
    req.extend((0..6).rev().map(|i| (5u8 >> i) & 1));
    req.push(0);
    for (i, h) in bits(&req).into_iter().enumerate() {
        b.write16(EWRAM + 2 * i as u32, h, N);
    }
    start_dma(&mut b, 3, EWRAM, 0x0DFF_FF00, 9, 0x8000);
    settle(&mut b);
    start_dma(&mut b, 3, 0x0DFF_FF00, EWRAM + 0x100, 68, 0x8000);
    settle(&mut b);
    let got = read_all16(&mut b, EWRAM + 0x100, 68);
    assert!(got[..4].iter().all(|&x| x & 1 == 0));
    let v = got[4..]
        .iter()
        .fold(0u64, |v, &x| v << 1 | u64::from(x & 1));
    assert_eq!(v, value);
}

#[test]
fn eeprom_8k_size_from_81_bit_dma() {
    let mut b = bus_with(rom_with(b"EEPROM_V124"));
    let mut req = vec![1u16, 0];
    req.extend((0..14).rev().map(|i| (0x155u16 >> i) & 1));
    req.extend((0..64).map(|_| 1u16));
    req.push(0);
    assert_eq!(req.len(), 81);
    for (i, h) in req.into_iter().enumerate() {
        b.write16(EWRAM + 2 * i as u32, h, N);
    }
    start_dma(&mut b, 3, EWRAM, 0x0D00_0000, 81, 0x8000);
    settle(&mut b);
    assert_eq!(b.cart.battery_ram().unwrap().len(), 8192);
    // All-ones data on an erased (0xFF) chip changes nothing.
    assert_eq!(b.cart.battery_generation(), 0);
}

#[test]
fn a_special_timing_channel_without_a_fifo_destination_is_not_sound_dma() {
    let mut b = bus();
    // DMA1 in special mode aimed at plain memory: never triggered by a FIFO
    // request, and (if it ever ran) it would use its own count, not 4 words.
    start_dma(
        &mut b,
        1,
        IWRAM,
        IWRAM + 0x100,
        1,
        0x8000 | 3 << 12 | 1 << 9,
    );
    b.dma.trigger(DmaTrigger::FifoA);
    b.dma.trigger(DmaTrigger::FifoB);
    assert!(b.dma.active_channel().is_none());
}
