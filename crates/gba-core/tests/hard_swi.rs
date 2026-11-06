//! Hostile SWI arguments (DECISIONS #68). A call runs inside one instruction step,
//! so every memory-moving SWI stops at a per-call cycle budget,
//! `gba_core::bios::SWI_CYCLE_CAP`, keeping what it wrote and charging only that.
//! Assertions are on emulated cycles, never wall-clock time.

mod sys_common;

use gba_core::bios::{SWI_CYCLE_CAP, swi};
use gba_core::bus::{Bus, CpuBus, SwiResult};
use sys_common::{N, bus};

const EWRAM: u32 = 0x0200_0000;
const IWRAM: u32 = 0x0300_0000;

/// Room for the last loop iteration, the 48-cycle call overhead and the header.
const SLACK: u64 = 2_000;

/// Call SWI `n` and return the cycles it took.
fn call(b: &mut Bus, n: u8, regs: [u32; 4]) -> u64 {
    let t = b.sched.now();
    let mut r = regs;
    assert_eq!(swi(b, n, &mut r), SwiResult::Done);
    b.sched.now() - t
}

fn put(b: &mut Bus, addr: u32, bytes: &[u8]) {
    for (i, &v) in bytes.iter().enumerate() {
        b.write8(addr + i as u32, v, N);
    }
}

/// The call stopped at the budget: it used all of it and no more than a loop
/// iteration over.
fn assert_stopped_at_the_cap(cycles: u64) {
    assert!(cycles >= SWI_CYCLE_CAP, "{cycles}");
    assert!(cycles <= SWI_CYCLE_CAP + SLACK, "{cycles}");
}

/// Entry-independent source: every byte 1, so any 20-byte (BG) or 8-byte (OBJ)
/// window of the mirrored IWRAM is the same entry and the output is the same
/// entry repeated, whichever way the destination wraps.
fn uniform_source(b: &mut Bus) {
    b.iwram.fill(1);
}

#[test]
fn bg_affine_set_with_a_4g_count_stops_at_the_cap() {
    let mut one = bus();
    uniform_source(&mut one);
    call(&mut one, 0x0E, [IWRAM, EWRAM, 1, 0]);
    let entry = one.ewram[..16].to_vec();
    assert_ne!(entry, [0; 16]);

    let mut b = bus();
    uniform_source(&mut b);
    let cycles = call(&mut b, 0x0E, [IWRAM, EWRAM, 0xFFFF_FFFF, 0]);
    assert_stopped_at_the_cap(cycles);
    // Documented output: entries done in order; at 16 bytes each the budget
    // wraps the 256 KiB destination several times over, so all of it is the entry.
    assert!(b.ewram.chunks(16).all(|c| c == entry), "the whole EWRAM");
}

#[test]
fn obj_affine_set_with_a_4g_count_stops_at_the_cap() {
    let mut one = bus();
    uniform_source(&mut one);
    call(&mut one, 0x0F, [IWRAM, EWRAM, 1, 2]);
    let entry = one.ewram[..8].to_vec();
    assert_ne!(entry, [0; 8]);

    let mut b = bus();
    uniform_source(&mut b);
    let cycles = call(&mut b, 0x0F, [IWRAM, EWRAM, 0xFFFF_FFFF, 2]);
    assert_stopped_at_the_cap(cycles);
    assert!(b.ewram.chunks(8).all(|c| c == entry), "the whole EWRAM");
}

#[test]
fn cpu_set_and_cpu_fast_set_at_their_maximum_counts_stop_at_the_cap() {
    // The 21-bit count field is the bound the profile relied on; at its maximum
    // (2 M units) a copy still costs more than the budget.
    let mut b = bus();
    b.ewram[..64].fill(0x5A);
    let cycles = call(
        &mut b,
        0x0B,
        [EWRAM, EWRAM + 0x1_0000, 0x1F_FFFF | 1 << 26, 0],
    );
    assert_stopped_at_the_cap(cycles);
    assert_eq!(b.peek32(EWRAM + 0x1_0000), 0x5A5A_5A5A);
    let cycles = call(&mut b, 0x0C, [EWRAM, EWRAM + 0x1_0000, 0x1F_FFFF, 0]);
    assert_stopped_at_the_cap(cycles);
}

#[test]
fn lz77_run_length_and_diff_with_a_16_mib_size_stop_at_the_cap() {
    let mut b = bus();
    // LZ77: a literal 0xAB, then references (length 18, distance 1) forever: the
    // flag byte 0x7F is one literal and seven references; repeat it.
    let mut lz = vec![0x10, 0xFF, 0xFF, 0xFF];
    for _ in 0..1000 {
        lz.extend_from_slice(&[0x7F, 0xAB]);
        for _ in 0..7 {
            lz.extend_from_slice(&[0xF0, 0x00]);
        }
    }
    put(&mut b, IWRAM, &lz);
    let cycles = call(&mut b, 0x11, [IWRAM, EWRAM, 0, 0]);
    assert_stopped_at_the_cap(cycles);
    assert_eq!(b.peek8(EWRAM), 0xAB);
    assert_eq!(b.peek8(EWRAM + 1000), 0xAB);

    // Run-length: runs of 130 bytes of 0x11.
    let mut b = bus();
    let mut rl = vec![0x30, 0xFF, 0xFF, 0xFF];
    for _ in 0..1000 {
        rl.extend_from_slice(&[0xFF, 0x11]);
    }
    put(&mut b, IWRAM, &rl);
    let cycles = call(&mut b, 0x14, [IWRAM, EWRAM, 0, 0]);
    assert_stopped_at_the_cap(cycles);
    assert!(b.ewram.contains(&0x11));

    // Diff unfilters, 8-bit and 16-bit.
    let mut b = bus();
    put(&mut b, IWRAM, &[0x81, 0xFF, 0xFF, 0xFF, 1, 1, 1, 1]);
    let cycles = call(&mut b, 0x16, [IWRAM, EWRAM, 0, 0]);
    assert_stopped_at_the_cap(cycles);
    assert!(b.ewram.iter().any(|&v| v != 0));
    put(&mut b, IWRAM, &[0x82, 0xFF, 0xFF, 0xFF, 1, 0, 1, 0]);
    let cycles = call(&mut b, 0x18, [IWRAM, EWRAM, 0, 0]);
    assert_stopped_at_the_cap(cycles);
}

#[test]
fn bit_unpack_with_the_largest_length_stops_at_the_cap() {
    let mut b = bus();
    // len 0xFFFF bytes, 1-bit units widened to 32 bits: 2 M words out. The zero
    // flag with offset 1 turns the zero source bits (IWRAM past the info block)
    // into 1s, so every word written is 1.
    put(&mut b, IWRAM, &[0xFF, 0xFF, 1, 32, 1, 0, 0, 0x80]);
    let cycles = call(&mut b, 0x10, [IWRAM + 0x100, EWRAM, IWRAM, 0]);
    assert_stopped_at_the_cap(cycles);
    // The output wrapped the 256 KiB destination mirror several times.
    assert!(b.ewram.chunks(4).all(|w| w == [1, 0, 0, 0]));
}

/// A Huffman stream with a degenerate tree: 255 nodes in a chain, each with a
/// leaf on its 0 side and the next node on its 1 side. A stream of ones walks
/// the whole chain for every leaf.
fn chain_tree(b: &mut Bus, size: u32) {
    b.ewram.fill(0xFF);
    let mut table = vec![0u8; 512];
    table[0] = 255; // (N / 2) - 1: a 512-byte table
    for i in (1..510).step_by(2) {
        table[i] = 0x80; // 0 side: leaf at i + 1; 1 side: the pair is the next node
        table[i + 1] = 1; // leaf data
    }
    table[509] = 0xC0; // last node: both sides are leaves, at 510 and 511
    table[510] = 0;
    table[511] = 1;
    let hdr = 0x20 | 1 | size << 8;
    put(b, EWRAM, &hdr.to_le_bytes());
    put(b, EWRAM + 4, &table);
}

#[test]
fn huffman_with_a_deep_tree_and_a_16_mib_size_stops_at_the_cap() {
    let mut b = bus();
    chain_tree(&mut b, 0xFF_FFFF);
    let dst = EWRAM + 0x2_0000;
    let cycles = call(&mut b, 0x13, [EWRAM, dst, 0, 0]);
    assert_stopped_at_the_cap(cycles);
    // Every leaf is data 1 (1-bit units): the words written are all ones.
    assert_eq!(b.peek32(dst), 0xFFFF_FFFF);
}

#[test]
fn huffman_stops_at_a_node_that_points_out_of_the_tree() {
    let mut b = bus();
    // A 4-byte table whose root points 63 pairs ahead, far past the table.
    put(
        &mut b,
        EWRAM,
        &[0x28, 0x10, 0, 0, 0x01, 0x3F, 0, 0, 0, 0, 0, 0],
    );
    put(&mut b, EWRAM + 0x100, &[0xEE; 8]);
    let cycles = call(&mut b, 0x13, [EWRAM, EWRAM + 0x100, 0, 0]);
    assert!(cycles < 500, "{cycles}");
    // Documented output: nothing is written for the malformed leaf.
    assert_eq!(b.peek32(EWRAM + 0x100), 0xEEEE_EEEE);
}

#[test]
fn huffman_leaf_data_outside_the_tree_is_malformed_too() {
    let mut b = bus();
    // Table of 4 bytes: root (index 1) has both children as leaves at offset 1,
    // i.e. at indexes 4 and 5, one pair past the table's end.
    put(
        &mut b,
        EWRAM,
        &[0x28, 0x10, 0, 0, 0x01, 0xC1, 0, 0, 0, 0, 0, 0],
    );
    put(&mut b, EWRAM + 0x100, &[0xEE; 8]);
    call(&mut b, 0x13, [EWRAM, EWRAM + 0x100, 0, 0]);
    assert_eq!(b.peek32(EWRAM + 0x100), 0xEEEE_EEEE);
}

#[test]
fn honest_large_calls_complete_inside_the_budget() {
    // CpuFastSet fill of all 256 KiB of EWRAM from IWRAM.
    let mut b = bus();
    b.write32(IWRAM, 0x1234_5678, N);
    let cycles = call(&mut b, 0x0C, [IWRAM, EWRAM, 0x1_0000 | 1 << 24, 0]);
    assert!(cycles < SWI_CYCLE_CAP / 2, "{cycles}");
    assert_eq!(b.peek32(EWRAM), 0x1234_5678);
    assert_eq!(b.peek32(EWRAM + 0x3_FFFC), 0x1234_5678);

    // LZ77 to 192 KiB of EWRAM, the source above it.
    let mut b = bus();
    let size = 0x3_0000u32;
    let src = EWRAM + 0x3_0000;
    let mut lz = (0x10 | size << 8).to_le_bytes().to_vec();
    for _ in 0..1600 {
        lz.extend_from_slice(&[0x7F, 0xAB]);
        for _ in 0..7 {
            lz.extend_from_slice(&[0xF0, 0x00]);
        }
    }
    put(&mut b, src, &lz);
    let cycles = call(&mut b, 0x11, [src, EWRAM, 0, 0]);
    assert!(cycles < SWI_CYCLE_CAP * 3 / 4, "{cycles}");
    assert!(b.ewram[..size as usize].iter().all(|&v| v == 0xAB));
}

/// Cycles of an honest call that writes `n` bytes (IWRAM to IWRAM, 1-cycle
/// accesses), to see what each extra output byte costs.
fn rl_cycles(n: u32) -> u64 {
    let mut b = bus();
    let mut rl = (0x30 | n << 8).to_le_bytes().to_vec();
    rl.extend_from_slice(&[0xFF, 0x11].repeat(n as usize / 130 + 2));
    put(&mut b, IWRAM, &rl);
    call(&mut b, 0x14, [IWRAM, IWRAM + 0x4000, 0, 0])
}

fn lz_cycles(n: u32) -> u64 {
    let mut b = bus();
    let mut lz = (0x10 | n << 8).to_le_bytes().to_vec();
    lz.extend_from_slice(&[0x7F, 0xAB]);
    lz.extend_from_slice(&[0xF0, 0x00].repeat(7 * (n as usize / 127 + 2)));
    put(&mut b, IWRAM, &lz);
    call(&mut b, 0x11, [IWRAM, IWRAM + 0x4000, 0, 0])
}

#[test]
fn decompression_compute_is_charged_per_output_byte() {
    // 4 cycles a byte of compute on top of the accesses, however the output is
    // produced (DECISIONS #65).
    assert!(rl_cycles(2000) - rl_cycles(1000) >= 1000 * 4);
    assert!(lz_cycles(2000) - lz_cycles(1000) >= 1000 * 4);
}

#[test]
fn huffman_charges_per_leaf_including_a_malformed_stop() {
    // Table of 4 bytes: root (index 1) has a leaf on its 0 side and the node at
    // index 3 on its 1 side; that node points far outside. 1-bit data. The stream
    // 0, 0, 0, 1, 1 decodes three leaves and then stops at the bad node.
    let mut b = bus();
    put(
        &mut b,
        EWRAM,
        &[0x21, 0x10, 0, 0, 0x01, 0x80, 0, 0, 0, 0, 0, 0x18],
    );
    let t = b.sched.now();
    call(&mut b, 0x13, [EWRAM, EWRAM + 0x100, 0, 0]);
    let cycles = b.sched.now() - t;
    // 48 call overhead, one header, one table-size byte and the stream word, the
    // node reads, and 3 cycles of compute for the three leaves.
    assert!((48 + 3 + 4..400).contains(&cycles), "{cycles}");
    assert_eq!(b.peek32(EWRAM + 0x100), 0, "no word was completed");
}
