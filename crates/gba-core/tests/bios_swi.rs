//! HLE BIOS SWIs, one call at a time through `bios::swi` on a bare bus. Vectors
//! come from GBATEK "BIOS Functions" examples where it has one (Div) and from
//! hand-built streams otherwise (see each test).

mod sys_common;

use gba_core::bios::swi;
use gba_core::bus::{Bus, CpuBus, SwiResult};
use sys_common::{N, bus};

const EWRAM: u32 = 0x0200_0000;
const VRAM: u32 = 0x0600_0000;
const BIOS_IF: u32 = 0x0300_7FF8;

fn call(bus: &mut Bus, n: u8, regs: [u32; 4]) -> (SwiResult, [u32; 4]) {
    let mut r = regs;
    let res = swi(bus, n, &mut r);
    (res, r)
}

fn put(bus: &mut Bus, addr: u32, bytes: &[u8]) {
    for (i, &b) in bytes.iter().enumerate() {
        bus.write8(addr + i as u32, b, N);
    }
}

fn get(bus: &mut Bus, addr: u32, n: usize) -> Vec<u8> {
    (0..n).map(|i| bus.peek8(addr + i as u32)).collect()
}

fn put32(bus: &mut Bus, addr: u32, words: &[u32]) {
    for (i, &w) in words.iter().enumerate() {
        bus.write32(addr + 4 * i as u32, w, N);
    }
}

#[test]
fn div_matches_the_gbatek_example() {
    let mut b = bus();
    let (res, r) = call(&mut b, 0x06, [-1234i32 as u32, 10, 0, 0]);
    assert_eq!(res, SwiResult::Done);
    assert_eq!(
        (r[0] as i32, r[1] as i32, r[3]),
        (-123, -4, 123),
        "GBATEK: -1234, 10 returns -123, -4, +123"
    );
    // DivArm has the operands exchanged.
    let (_, r) = call(&mut b, 0x07, [10, -1234i32 as u32, 0, 0]);
    assert_eq!((r[0] as i32, r[1] as i32, r[3]), (-123, -4, 123));
    let (_, r) = call(&mut b, 0x06, [i32::MIN as u32, -1i32 as u32, 0, 0]);
    assert_eq!((r[0], r[1], r[3]), (0x8000_0000, 0, 0x8000_0000));
}

#[test]
fn div_by_zero_hangs_halted_and_charges_time() {
    let mut b = bus();
    let t = b.sched.now();
    let (res, _) = call(&mut b, 0x06, [5, 0, 0, 0]);
    assert_eq!(res, SwiResult::WaitThenRetry);
    assert!(b.halted);
    assert!(b.sched.now() > t, "a retry loop must advance the clock");
}

#[test]
fn sqrt_arctan_and_checksum() {
    let mut b = bus();
    assert_eq!(call(&mut b, 0x08, [99, 0, 0, 0]).1[0], 9);
    assert_eq!(call(&mut b, 0x08, [0xFFFF_FFFF, 0, 0, 0]).1[0], 65535);
    assert_eq!(call(&mut b, 0x09, [0x4000, 0, 0, 0]).1[0], 0x2000);
    assert_eq!(
        call(&mut b, 0x0A, [0x4000, 0x4000, 0, 0]).1[0],
        0x2000,
        "x = y = 1.0 is 45 degrees"
    );
    assert_eq!(call(&mut b, 0x0A, [0xC000, 0, 0, 0]).1[0], 0x8000); // x = -1.0 (16-bit), y = 0
    assert_eq!(call(&mut b, 0x0D, [0; 4]).1[0], 0xBAAE_187F);
}

#[test]
fn every_done_call_sets_the_after_swi_latch() {
    let mut b = bus();
    b.bios_latch = 0;
    call(&mut b, 0x08, [4, 0, 0, 0]);
    assert_eq!(b.bios_latch, 0xE3A0_2004);
}

#[test]
fn cpu_set_copies_and_fills_both_widths() {
    let mut b = bus();
    put(&mut b, EWRAM, &[1, 2, 3, 4, 5, 6, 7, 8]);
    // 16-bit copy of 3 halfwords.
    call(&mut b, 0x0B, [EWRAM, EWRAM + 0x100, 3, 0]);
    assert_eq!(get(&mut b, EWRAM + 0x100, 8), [1, 2, 3, 4, 5, 6, 0, 0]);
    // 32-bit fill (bit 24) of 3 words from the first word.
    call(
        &mut b,
        0x0B,
        [EWRAM, EWRAM + 0x200, 3 | 1 << 24 | 1 << 26, 0],
    );
    assert_eq!(
        get(&mut b, EWRAM + 0x200, 13),
        [1, 2, 3, 4, 1, 2, 3, 4, 1, 2, 3, 4, 0]
    );
    // 16-bit fill.
    call(&mut b, 0x0B, [EWRAM, EWRAM + 0x300, 2 | 1 << 24, 0]);
    assert_eq!(get(&mut b, EWRAM + 0x300, 6), [1, 2, 1, 2, 0, 0]);
}

#[test]
fn cpu_set_rejects_a_bios_source() {
    let mut b = bus();
    put(&mut b, EWRAM, &[0xAA; 8]);
    call(&mut b, 0x0B, [0x100, EWRAM, 2 | 1 << 26, 0]);
    call(&mut b, 0x0C, [0x100, EWRAM, 8, 0]);
    assert_eq!(get(&mut b, EWRAM, 8), [0xAA; 8]);
}

#[test]
fn cpu_fast_set_rounds_up_to_eight_words() {
    let mut b = bus();
    put32(&mut b, EWRAM, &[0xDEAD_BEEF]);
    call(&mut b, 0x0C, [EWRAM, EWRAM + 0x100, 3 | 1 << 24, 0]);
    for i in 0..8 {
        assert_eq!(b.peek32(EWRAM + 0x100 + 4 * i), 0xDEAD_BEEF, "word {i}");
    }
    assert_eq!(b.peek32(EWRAM + 0x120), 0);
    // Copy mode, 8 words.
    let src: Vec<u32> = (1..=8).collect();
    put32(&mut b, EWRAM + 0x400, &src);
    call(&mut b, 0x0C, [EWRAM + 0x400, EWRAM + 0x500, 8, 0]);
    assert_eq!(b.peek32(EWRAM + 0x51C), 8);
}

#[test]
fn lz77_wram_and_vram() {
    let mut b = bus();
    // 8 bytes: literal 'A', then 7 bytes copied from 1 back (disp field 0).
    put(&mut b, EWRAM, &[0x10, 8, 0, 0, 0x40, b'A', 0x40, 0x00]);
    call(&mut b, 0x11, [EWRAM, EWRAM + 0x100, 0, 0]);
    assert_eq!(get(&mut b, EWRAM + 0x100, 9), b"AAAAAAAA\0");
    // 6 bytes "AB" then a 4-byte copy from 2 back, written to VRAM as halfwords.
    put(
        &mut b,
        EWRAM + 0x40,
        &[0x10, 6, 0, 0, 0x20, b'A', b'B', 0x10, 0x01],
    );
    call(&mut b, 0x12, [EWRAM + 0x40, VRAM, 0, 0]);
    assert_eq!(get(&mut b, VRAM, 6), b"ABABAB");
}

#[test]
fn lz77_vram_back_reference_to_the_pending_byte_reads_stale_data() {
    // GBATEK: the Vram variant "works only with disp=001h..FFFh", not 000h.
    let mut b = bus();
    put(&mut b, VRAM, &[0x77, 0x77]);
    put(&mut b, EWRAM, &[0x10, 4, 0, 0, 0x40, b'A', 0x10, 0x00]);
    call(&mut b, 0x12, [EWRAM, VRAM + 0x10, 0, 0]);
    // Pending 'A' is not in memory yet when the copy reads dest-1.
    let out = get(&mut b, VRAM + 0x10, 4);
    assert_eq!(out[0], b'A');
    assert_ne!(out[1], b'A');
}

#[test]
fn run_length_expands_both_kinds_of_run() {
    let mut b = bus();
    put(&mut b, EWRAM, &[0x30, 7, 0, 0, 0x81, b'X', 0x02, 1, 2, 3]);
    call(&mut b, 0x14, [EWRAM, EWRAM + 0x100, 0, 0]);
    assert_eq!(
        get(&mut b, EWRAM + 0x100, 8),
        [b'X', b'X', b'X', b'X', 1, 2, 3, 0]
    );
    call(&mut b, 0x15, [EWRAM, VRAM, 0, 0]);
    assert_eq!(get(&mut b, VRAM, 6), [b'X', b'X', b'X', b'X', 1, 2]);
}

#[test]
fn huffman_decodes_a_two_leaf_tree() {
    let mut b = bus();
    // 8-bit data, 4 bytes out. Table: root 0xC0 (both children are leaves,
    // offset 0), leaves 'a' and 'b'. Bits 0,1,1,0 (bit 31 first) -> "abba".
    put(
        &mut b,
        EWRAM,
        &[0x28, 0x04, 0, 0, 0x01, 0xC0, b'a', b'b', 0, 0, 0, 0x60],
    );
    call(&mut b, 0x13, [EWRAM, EWRAM + 0x100, 0, 0]);
    assert_eq!(get(&mut b, EWRAM + 0x100, 4), b"abba");
}

#[test]
fn diff_unfilters_accumulate() {
    let mut b = bus();
    put(&mut b, EWRAM, &[0x81, 4, 0, 0, 10, 1, 1, 1]);
    call(&mut b, 0x16, [EWRAM, EWRAM + 0x100, 0, 0]);
    assert_eq!(get(&mut b, EWRAM + 0x100, 4), [10, 11, 12, 13]);
    call(&mut b, 0x17, [EWRAM, VRAM, 0, 0]);
    assert_eq!(get(&mut b, VRAM, 4), [10, 11, 12, 13]);
    put(
        &mut b,
        EWRAM + 0x40,
        &[0x82, 6, 0, 0, 100, 0, 1, 0, 0xFF, 0xFF],
    );
    call(&mut b, 0x18, [EWRAM + 0x40, EWRAM + 0x140, 0, 0]);
    assert_eq!(b.peek16(EWRAM + 0x140), 100);
    assert_eq!(b.peek16(EWRAM + 0x142), 101);
    assert_eq!(b.peek16(EWRAM + 0x144), 100);
}

#[test]
fn bit_unpack_widens_one_bit_units_to_nibbles() {
    let mut b = bus();
    put(&mut b, EWRAM, &[0b1011_0001]);
    // len 1, source width 1, destination width 4, offset 0.
    put(&mut b, EWRAM + 0x10, &[1, 0, 1, 4, 0, 0, 0, 0]);
    call(&mut b, 0x10, [EWRAM, EWRAM + 0x100, EWRAM + 0x10, 0]);
    assert_eq!(b.peek32(EWRAM + 0x100), 0x1011_0001);
    // With offset 3 and the zero flag every unit gains 3.
    put(&mut b, EWRAM + 0x20, &[1, 0, 1, 4, 3, 0, 0, 0x80]);
    call(&mut b, 0x10, [EWRAM, EWRAM + 0x200, EWRAM + 0x20, 0]);
    assert_eq!(b.peek32(EWRAM + 0x200), 0x4344_3334);
}

#[test]
fn affine_sets_use_the_bios_sine_table() {
    let mut b = bus();
    // ObjAffineSet: scale 1.0, angle 0 then 90 degrees, contiguous (offset 2).
    put32(&mut b, EWRAM, &[0x0100_0100, 0x0000, 0x0100_0100, 0x4000]);
    call(&mut b, 0x0F, [EWRAM, EWRAM + 0x100, 2, 2]);
    let p: Vec<i16> = (0..8)
        .map(|i| b.peek16(EWRAM + 0x100 + 2 * i) as i16)
        .collect();
    assert_eq!(p, [0x100, 0, 0, 0x100, 0, -0x100, 0x100, 0]);
    // OAM-style spacing (offset 8).
    call(&mut b, 0x0F, [EWRAM, EWRAM + 0x200, 1, 8]);
    assert_eq!(b.peek16(EWRAM + 0x200), 0x100);
    assert_eq!(b.peek16(EWRAM + 0x218), 0x100);
    // BgAffineSet: centre (100, 80) maps to display (120, 80), unrotated 1:1.
    put32(
        &mut b,
        EWRAM + 0x300,
        &[100 << 8, 80 << 8, 80 << 16 | 120, 0x0100_0100, 0],
    );
    call(&mut b, 0x0E, [EWRAM + 0x300, EWRAM + 0x400, 1, 0]);
    assert_eq!(b.peek16(EWRAM + 0x400), 0x100);
    assert_eq!(b.peek16(EWRAM + 0x406), 0x100);
    assert_eq!(b.peek32(EWRAM + 0x408) as i32, 100 * 256 - 120 * 256);
    assert_eq!(b.peek32(EWRAM + 0x40C) as i32, 0);
}

#[test]
fn register_ram_reset_honours_each_flag() {
    let mut b = bus();
    b.write32(0x0300_0000, 0x1234, N);
    b.write32(0x0300_7F00, 0x5678, N);
    b.write32(EWRAM, 0x9ABC, N);
    b.write16(VRAM, 0x4321, N);
    call(&mut b, 0x01, [0x02, 0, 0, 0]);
    assert_eq!(b.peek32(0x0300_0000), 0);
    assert_eq!(b.peek32(0x0300_7F00), 0x5678, "the top 0x200 bytes survive");
    assert_eq!(b.peek32(EWRAM), 0x9ABC);
    assert_eq!(b.peek16(VRAM), 0x4321);
    assert_eq!(b.read_io16(0x00), 0x80, "forced blank whatever the flags");
    call(&mut b, 0x01, [0x01 | 0x08, 0, 0, 0]);
    assert_eq!(b.peek32(EWRAM), 0);
    assert_eq!(b.peek16(VRAM), 0);
    // Flag 7 clears the video registers (PA/PD are write-only, so the 0x100
    // identity they get back cannot be read here) and IE.
    b.write_io16(0x08, 0x1234, 0xFFFF);
    b.write_io16(0x200, 0x3FFF, 0xFFFF);
    call(&mut b, 0x01, [0x80, 0, 0, 0]);
    assert_eq!(b.read_io16(0x08), 0);
    assert_eq!(b.read_io16(0x200), 0);
}

#[test]
fn sound_bias_sets_the_level_and_keeps_upper_bits() {
    let mut b = bus();
    b.write_io16(0x88, 0xC200, 0xFFFF);
    call(&mut b, 0x19, [0, 0, 0, 0]);
    assert_eq!(b.read_io16(0x88) & 0x3FF, 0);
    call(&mut b, 0x19, [1, 0, 0, 0]);
    assert_eq!(b.read_io16(0x88) & 0x3FF, 0x200);
    assert_eq!(b.read_io16(0x88) & 0xC000, 0xC000);
}

#[test]
fn halt_stop_and_custom_halt_write_haltcnt() {
    let mut b = bus();
    assert_eq!(call(&mut b, 0x02, [0; 4]).0, SwiResult::Done);
    assert!(b.halted && !b.stopped);
    b.halted = false;
    call(&mut b, 0x03, [0; 4]);
    assert!(b.halted && b.stopped);
    b.halted = false;
    b.stopped = false;
    call(&mut b, 0x27, [0, 0, 0x80, 0]);
    assert!(b.halted && b.stopped);
}

#[test]
fn halt_returns_at_once_if_an_enabled_irq_is_pending() {
    let mut b = bus();
    b.irq.ie = 1;
    b.irq.if_ = 1;
    call(&mut b, 0x02, [0; 4]);
    assert!(!b.halted);
}

#[test]
fn intr_wait_protocol_discard_wait_and_consume() {
    let mut b = bus();
    // An old VBlank flag with r0 = 0 returns at once and consumes the flag.
    b.write16(BIOS_IF, 0x0001, N);
    let (res, _) = call(&mut b, 0x04, [0, 1, 0, 0]);
    assert_eq!(res, SwiResult::Done);
    assert_eq!(b.peek16(BIOS_IF), 0);
    assert!(b.irq.ime, "IntrWait forces IME = 1");
    // r0 = 1 discards the old flag, then waits; r2 carries the retry marker.
    b.write16(BIOS_IF, 0x0003, N);
    let (res, r) = call(&mut b, 0x04, [1, 1, 0, 0]);
    assert_eq!(res, SwiResult::WaitThenRetry);
    assert_ne!(r[2], 0);
    assert_eq!(
        b.peek16(BIOS_IF),
        0x0002,
        "only the awaited bit was discarded"
    );
    assert!(b.halted);
    // Retry without a new flag keeps waiting; a new flag finishes and clears it
    // (the retry must not discard it again, r0 is still 1).
    assert_eq!(call(&mut b, 0x04, r).0, SwiResult::WaitThenRetry);
    b.write16(BIOS_IF, 0x0003, N);
    assert_eq!(call(&mut b, 0x04, r).0, SwiResult::Done);
    assert_eq!(b.peek16(BIOS_IF), 0x0002);
    // VBlankIntrWait is IntrWait(1, 1).
    b.write16(BIOS_IF, 0x0001, N);
    let (res, r) = call(&mut b, 0x05, [0; 4]);
    assert_eq!((res, r[0], r[1]), (SwiResult::WaitThenRetry, 1, 1));
    b.write16(BIOS_IF, 0x0001, N);
    assert_eq!(call(&mut b, 0x05, r).0, SwiResult::Done);
}

#[test]
fn unknown_and_reset_swis_are_left_to_the_image() {
    let mut b = bus();
    for n in [0x00, 0x2B, 0xFF] {
        assert_eq!(call(&mut b, n, [0; 4]).0, SwiResult::Unhandled, "{n:#x}");
    }
    // HardReset wipes RAM and the soft-reset flag first.
    b.write8(0x0300_7FFA, 1, N);
    b.write32(EWRAM, 7, N);
    assert_eq!(call(&mut b, 0x26, [0; 4]).0, SwiResult::Unhandled);
    assert_eq!((b.peek8(0x0300_7FFA), b.peek32(EWRAM)), (0, 0));
}

#[test]
fn sound_driver_calls_are_no_ops_and_multiboot_fails() {
    let mut b = bus();
    for n in [0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x20, 0x24, 0x28, 0x29, 0x2A] {
        let (res, r) = call(&mut b, n, [1, 2, 3, 4]);
        assert_eq!((res, r), (SwiResult::Done, [1, 2, 3, 4]), "{n:#x}");
    }
    assert_eq!(call(&mut b, 0x25, [0; 4]).1[0], 1);
}

#[test]
fn memory_calls_charge_the_bus_per_access() {
    let mut b = bus();
    let t = b.sched.now();
    call(&mut b, 0x0C, [EWRAM, EWRAM + 0x100, 8, 0]);
    let small = b.sched.now() - t;
    let t = b.sched.now();
    call(&mut b, 0x0C, [EWRAM, EWRAM + 0x100, 64, 0]);
    let large = b.sched.now() - t;
    assert!(large > small * 4, "{small} vs {large}");
}
