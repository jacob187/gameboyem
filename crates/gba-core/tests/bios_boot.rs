//! Whole-machine BIOS tests: hand-assembled ARM programs that use the BIOS IRQ
//! path, VBlankIntrWait and SoftReset, jsmolka's `bios.gba` under the HLE image,
//! and (only when `GBA_BIOS` points at a 16 KiB dump) the same ROM under a real
//! BIOS.

use gba_core::Gba;
use std::path::Path;

// ---- a tiny ARM assembler for the test programs ---------------------------------

const AL: u32 = 0xE000_0000;

fn imm(v: u32) -> u32 {
    (0..16)
        .find_map(|rot| {
            let r = v.rotate_left(rot * 2);
            (r <= 0xFF).then_some(rot << 8 | r)
        })
        .expect("immediate not encodable")
}

fn dp(op: u32, s: u32, rn: u32, rd: u32, v: u32) -> u32 {
    AL | 1 << 25 | op << 21 | s << 20 | rn << 16 | rd << 12 | imm(v)
}
fn mov(rd: u32, v: u32) -> u32 {
    dp(13, 0, 0, rd, v)
}
fn add(rd: u32, rn: u32, v: u32) -> u32 {
    dp(4, 0, rn, rd, v)
}
fn cmp(rn: u32, v: u32) -> u32 {
    dp(10, 1, rn, 0, v)
}
fn orr_rr(rd: u32, rn: u32, rm: u32) -> u32 {
    AL | 12 << 21 | rn << 16 | rd << 12 | rm
}
fn str_i(rd: u32, rn: u32, off: i32) -> u32 {
    mem(false, false, rd, rn, off)
}
fn ldr_i(rd: u32, rn: u32, off: i32) -> u32 {
    mem(true, false, rd, rn, off)
}
fn strb_i(rd: u32, rn: u32, off: i32) -> u32 {
    mem(false, true, rd, rn, off)
}
fn mem(load: bool, byte: bool, rd: u32, rn: u32, off: i32) -> u32 {
    let (u, m) = if off >= 0 {
        (1, off as u32)
    } else {
        (0, (-off) as u32)
    };
    AL | 1 << 26
        | 1 << 24
        | u << 23
        | (byte as u32) << 22
        | (load as u32) << 20
        | rn << 16
        | rd << 12
        | m
}
fn half(load: bool, rd: u32, rn: u32, off: u32) -> u32 {
    AL | 1 << 24
        | 1 << 23
        | 1 << 22
        | (load as u32) << 20
        | rn << 16
        | rd << 12
        | (off >> 4) << 8
        | 0xB0
        | off & 0xF
}
fn swi(n: u32) -> u32 {
    AL | 0x0F00_0000 | n << 16
}
const BX_LR: u32 = 0xE12F_FF1E;
fn b(at: u32, to: u32, cond: u32) -> u32 {
    cond << 28 | 0x0A00_0000 | ((to.wrapping_sub(at + 8) >> 2) & 0xFF_FFFF)
}

/// A ROM of `words` placed at 0x08000000. Fields past the program are zero.
fn rom(words: &[u32]) -> Vec<u8> {
    let mut rom = vec![0u8; 0x400];
    for (i, w) in words.iter().enumerate() {
        rom[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    rom
}

const BASE: u32 = 0x0800_0000;

/// Run until PC reaches `idle` or `frames` frames pass; returns whether it did.
fn run_to(gba: &mut Gba, idle: u32, frames: u32) -> bool {
    for _ in 0..frames {
        for _ in 0..400_000 {
            if gba.cpu_registers().r[15] == idle {
                return true;
            }
            gba.step_instruction();
        }
    }
    gba.cpu_registers().r[15] == idle
}

/// Install `handler` at 0x03007FFC, enable the VBlank IRQ and IME. Leaves r0 =
/// 0x04000000. 11 words.
fn irq_setup(handler: u32) -> Vec<u32> {
    vec![
        mov(0, 0x0400_0000),
        add(1, 15, handler - (BASE + 4 + 8)), // r1 = handler (pc-relative)
        str_i(1, 0, -4),                      // [0x03FFFFFC] = handler
        mov(1, 8),
        half(false, 1, 0, 4), //                 DISPSTAT: VBlank IRQ enable
        mov(1, 1),
        add(2, 0, 0x200),
        half(false, 1, 2, 0), //                 IE = VBlank
        half(false, 1, 2, 8), //                 IME = 1
        mov(5, 0),
        mov(1, 0),
    ]
}

/// The IRQ handler of a libgba-style program: acknowledge IF and OR it into the
/// BIOS interrupt flags at 0x03007FF8. Returns to the BIOS stub with `bx lr`.
fn irq_handler() -> Vec<u32> {
    vec![
        mov(0, 0x0400_0000),
        add(0, 0, 0x200),
        half(true, 1, 0, 2),  // r1 = IF
        half(false, 1, 0, 2), // acknowledge
        mov(2, 0x0300_0000),
        add(2, 2, 0x7F00),
        add(2, 2, 0xF8),
        half(true, 3, 2, 0),
        orr_rr(3, 3, 1),
        half(false, 3, 2, 0),
        BX_LR,
    ]
}

#[test]
fn vblank_intr_wait_through_the_bios_irq_path() {
    // r5 counts three VBlankIntrWait calls, then the program idles.
    let setup_len = 11;
    let loop_at = BASE + 4 * setup_len;
    let idle_at = loop_at + 4 * 4;
    let handler_at = idle_at + 4;
    let mut p = irq_setup(handler_at);
    p.extend([
        swi(5),
        add(5, 5, 1),
        cmp(5, 3),
        b(loop_at + 12, loop_at, 1), // bne loop
        b(idle_at, idle_at, 14),     // b .
    ]);
    // `idle_at` must be the `b .` word: the loop is 4 words, so it is p[15].
    assert_eq!(p.len() as u32 * 4 + BASE, idle_at + 4);
    p.extend(irq_handler());
    let mut gba = Gba::new(rom(&p), None).unwrap();
    let ok = run_to(&mut gba, idle_at, 8);
    let r = gba.cpu_registers();
    assert!(
        ok,
        "never reached the idle loop: pc={:#x} r5={} cpsr={:#x} frames={} ie={:#x} if={:#x} biosif={:#x} h={:#x}",
        r.r[15],
        r.r[5],
        r.cpsr,
        gba.frames(),
        gba.peek16(0x0400_0200),
        gba.peek16(0x0400_0202),
        gba.peek16(0x0300_7FF8),
        gba.peek32(0x0300_7FFC)
    );
    assert_eq!(gba.cpu_registers().r[5], 3);
    // Each wait consumed a whole frame: three frames, give or take the first.
    assert!((3..=4).contains(&gba.frames()), "{} frames", gba.frames());
    // The BIOS flags were consumed and the CPU is back in System mode.
    assert_eq!(gba.peek16(0x0300_7FF8) & 1, 0);
    assert_eq!(gba.cpu_registers().cpsr & 0x1F, 0x1F);
}

#[test]
fn soft_reset_runs_the_images_code_and_honours_the_return_flag() {
    // First run: counter in EWRAM, some state, then SWI 0. SoftReset enters the
    // ROM again (flag clear); the counter is then 2 and the program idles.
    let idle = BASE + 4 * 12;
    let p = vec![
        mov(0, 0x0200_0000),
        ldr_i(1, 0, 0x100),
        add(1, 1, 1),
        str_i(1, 0, 0x100),
        cmp(1, 2),
        b(BASE + 20, idle, 0), // beq idle
        mov(4, 0x0300_0000),   // state SoftReset must wipe
        add(4, 4, 0x7E00),
        str_i(4, 4, 0x10),
        mov(7, 7),
        swi(0),
        b(idle, idle, 14), // not reached: SoftReset does not return
        b(idle, idle, 14), // idle: b .
    ];
    let mut gba = Gba::new(rom(&p), None).unwrap();
    assert!(run_to(&mut gba, idle, 4));
    let r = gba.cpu_registers();
    assert_eq!(gba.peek32(0x0200_0100), 2, "the program ran twice");
    assert_eq!(gba.peek32(0x0300_7E10), 0, "0x03007E00.. was cleared");
    assert_eq!(r.cpsr & 0x1F, 0x1F, "System mode");
    assert_eq!(r.r[13], 0x0300_7F00);
    assert_eq!((r.r[7], r.r[4]), (0, 0), "registers were zeroed");
}

#[test]
fn soft_reset_flag_selects_ewram() {
    // Put `b .` (0xEAFFFFFE) at 0x02000000, set the flag byte, SoftReset.
    let p = vec![
        mov(0, 0x0200_0000),
        mov(1, 0xEA00_0000),
        add(1, 1, 0x00FF_0000),
        add(1, 1, 0xFF00),
        add(1, 1, 0xFE),
        str_i(1, 0, 0),
        mov(2, 0x0300_0000),
        add(2, 2, 0x7F00),
        mov(3, 1),
        strb_i(3, 2, 0xFA),
        swi(0),
    ];
    let mut gba = Gba::new(rom(&p), None).unwrap();
    assert!(run_to(&mut gba, 0x0200_0000, 4), "did not enter EWRAM");
    assert_eq!(gba.cpu_registers().r[13], 0x0300_7F00);
}

// ---- jsmolka's bios.gba ----------------------------------------------------------

const JSMOLKA_BIOS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../roms/test/gba/jsmolka/bios/bios.gba"
);
/// The `b .` after the result display (gba-facts.md 1a).
const BIOS_GBA_IDLE: u32 = 0x0800_03C0;

fn run_bios_gba(bios: Option<Box<[u8]>>, frames: u32) -> Option<u32> {
    let rom = std::fs::read(Path::new(JSMOLKA_BIOS)).ok()?;
    let mut gba = Gba::new(rom, bios).unwrap();
    for _ in 0..frames {
        gba.step_frame();
        if gba.cpu_registers().r[15] == BIOS_GBA_IDLE {
            return Some(gba.cpu_registers().r[12]);
        }
    }
    panic!("bios.gba never reached its idle loop");
}

#[test]
fn bios_gba_latch_values_under_the_hle_image() {
    // The image's words reproduce the read-protection values bios.gba expects:
    // after startup, after an SWI, during the IRQ handler and after it.
    match run_bios_gba(None, 30) {
        None => eprintln!("skipping: roms/test/gba/jsmolka/bios/bios.gba not present"),
        Some(r12) => assert_eq!(r12, 0, "bios.gba failed test {r12}"),
    }
}

#[test]
fn bios_gba_passes_with_a_real_bios() {
    let Some(path) = std::env::var_os("GBA_BIOS") else {
        eprintln!("skipping: GBA_BIOS is not set");
        return;
    };
    let dump = std::fs::read(&path).expect("GBA_BIOS is not readable");
    if dump.len() != gba_core::BIOS_SIZE {
        eprintln!("skipping: GBA_BIOS is not a 16 KiB file");
        return;
    }
    // The real BIOS plays the boot logo first (about 3 seconds).
    match run_bios_gba(Some(dump.into_boxed_slice()), 600) {
        None => eprintln!("skipping: roms/test/gba/jsmolka/bios/bios.gba not present"),
        Some(r12) => assert_eq!(r12, 0, "bios.gba failed test {r12} with the real BIOS"),
    }
}

#[test]
fn a_real_bios_dump_is_read_through_protection() {
    // A synthetic "dump": the first word is a branch to the cartridge entry via
    // a literal (so the machine boots through it), the rest a recognisable
    // pattern. Reads from the ROM see the latch, a peek sees the dump.
    let mut dump = vec![0u8; gba_core::BIOS_SIZE];
    for (i, b) in dump.iter_mut().enumerate() {
        *b = (i * 7) as u8;
    }
    dump[0..4].copy_from_slice(&0xE59F_F018u32.to_le_bytes()); // ldr pc, [pc, #0x18]
    dump[0x20..0x24].copy_from_slice(&BASE.to_le_bytes());
    // ROM: r1 = [0] (a protected BIOS read), then idle.
    let p = [mov(0, 0), ldr_i(1, 0, 0), b(BASE + 8, BASE + 8, 14)];
    let mut gba = Gba::new(rom(&p), Some(dump.clone().into_boxed_slice())).unwrap();
    assert!(run_to(&mut gba, BASE + 8, 2));
    let latch = u32::from_le_bytes([dump[8], dump[9], dump[10], dump[11]]);
    assert_eq!(gba.peek32(0), 0xE59F_F018, "peek bypasses protection");
    // The load saw the last word fetched from the BIOS: the second prefetch of
    // `ldr pc` (0x08), not the instruction itself.
    assert_eq!(gba.cpu_registers().r[1], latch);
}
