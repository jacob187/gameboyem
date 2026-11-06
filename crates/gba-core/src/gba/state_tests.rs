//! Save-state tests that need the machine's internals: the scenarios where a
//! state is taken in the middle of something (a flash command, an EEPROM
//! transfer, a DMA, an HLE wait, a filled prefetch buffer), the header and
//! rollback rules, structural corruption and the size cap. Whole-ROM
//! determinism lives in `gb-test/tests/gba_save_state.rs`.

use super::*;
use crate::bus::{Access, CpuBus};

const N: Access = Access::NonSeq;
const BASE: u32 = 0x0800_0000;

// ---- a tiny ARM assembler (as in tests/bios_boot.rs) ----------------------------

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
fn str_i(rd: u32, rn: u32, off: i32) -> u32 {
    let (u, m) = if off >= 0 {
        (1, off as u32)
    } else {
        (0, (-off) as u32)
    };
    AL | 1 << 26 | 1 << 24 | u << 23 | rn << 16 | rd << 12 | m
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
fn orr_rr(rd: u32, rn: u32, rm: u32) -> u32 {
    AL | 12 << 21 | rn << 16 | rd << 12 | rm
}
fn swi(n: u32) -> u32 {
    AL | 0x0F00_0000 | n << 16
}
fn b(at: u32, to: u32) -> u32 {
    AL | 0x0A00_0000 | ((to.wrapping_sub(at + 8) >> 2) & 0xFF_FFFF)
}
const BX_LR: u32 = 0xE12F_FF1E;
const B_SELF: u32 = 0xEAFF_FFFE;
/// `ldmia r0, {r1-r8}`: eight data cycles in which the cartridge bus is idle.
const LDM_R1_R8: u32 = 0xE890_01FE;

/// A ROM of `words` at 0x08000000, `id` (a save-library string) at 0x100 if
/// the program is shorter than that.
fn rom(words: &[u32], id: &[u8]) -> Vec<u8> {
    let mut rom = vec![0u8; 0x400];
    for (i, w) in words.iter().enumerate() {
        rom[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    assert!(words.len() * 4 <= 0x100);
    rom[0x100..0x100 + id.len()].copy_from_slice(id);
    rom
}

/// VBlankIntrWait forever through the BIOS IRQ path (libgba-style handler);
/// r5 counts the waits.
fn intr_wait_program(id: &[u8]) -> Vec<u8> {
    let loop_at = BASE + 4 * 11;
    let handler = loop_at + 12;
    let mut p = vec![
        mov(0, 0x0400_0000),
        add(1, 15, handler - (BASE + 4 + 8)),
        str_i(1, 0, -4), // [0x03FFFFFC] = handler
        mov(1, 8),
        half(false, 1, 0, 4), // DISPSTAT: VBlank IRQ enable
        mov(1, 1),
        add(2, 0, 0x200),
        half(false, 1, 2, 0), // IE = VBlank
        half(false, 1, 2, 8), // IME = 1
        mov(5, 0),
        mov(1, 0),
    ];
    p.extend([swi(5), add(5, 5, 1), b(loop_at + 8, loop_at)]);
    p.extend([
        mov(0, 0x0400_0000), // handler: acknowledge IF, OR it into the BIOS flags
        add(0, 0, 0x200),
        half(true, 1, 0, 2),
        half(false, 1, 0, 2),
        mov(2, 0x0300_0000),
        add(2, 2, 0x7F00),
        add(2, 2, 0xF8),
        half(true, 3, 2, 0),
        orr_rr(3, 3, 1),
        half(false, 3, 2, 0),
        BX_LR,
    ]);
    rom(&p, id)
}

fn gba(rom: &[u8]) -> Gba {
    Gba::new(rom.to_vec(), None).unwrap()
}

// ---- comparing machines ---------------------------------------------------------

/// A fresh machine loaded from `a`'s state; the load must reproduce it exactly.
fn fork(a: &Gba, mut fresh: Gba) -> Gba {
    let snap = a.save_state();
    fresh.load_state(&snap).expect("a fresh state loads");
    assert!(fresh.save_state() == snap, "load then save is the identity");
    fresh
}

fn same(a: &Gba, b: &Gba, what: &str) {
    assert_eq!(a.cycles(), b.cycles(), "{what}: cycles");
    assert_eq!(a.cpu_registers(), b.cpu_registers(), "{what}: registers");
    assert!(a.save_state() == b.save_state(), "{what}: whole state");
}

fn step_both(a: &mut Gba, b: &mut Gba, steps: usize) {
    for _ in 0..steps {
        a.step_instruction();
        b.step_instruction();
    }
}

fn run_dma(g: &mut Gba) {
    for _ in 0..100_000 {
        if g.bus.dma.active_channel().is_none() {
            return;
        }
        g.step_instruction();
    }
    panic!("DMA never finished");
}

fn dma3(g: &mut Gba, src: u32, dst: u32, count: u16, control: u16) {
    g.bus.write32(0x0400_00D4, src, N);
    g.bus.write32(0x0400_00D8, dst, N);
    g.bus
        .write32(0x0400_00DC, u32::from(count) | u32::from(control) << 16, N);
}

// ---- scenarios -------------------------------------------------------------------

#[test]
fn a_state_resumes_identically_at_many_points() {
    let rom = intr_wait_program(b"");
    let mut a = gba(&rom);
    for round in 0..5 {
        for _ in 0..37_001 {
            a.step_instruction();
        }
        a.drain_audio_into(&mut Vec::new());
        let mut b = fork(&a, gba(&rom));
        step_both(&mut a, &mut b, 20_000);
        same(&a, &b, &format!("round {round}"));
        let (mut sa, mut sb) = (Vec::new(), Vec::new());
        a.drain_audio_into(&mut sa);
        b.drain_audio_into(&mut sb);
        assert_eq!(sa.len(), sb.len());
        assert!(
            sa.iter().zip(&sb).all(|(x, y)| x.to_bits() == y.to_bits()),
            "audio differs in round {round}"
        );
    }
    assert!(a.frames() > 5 && a.cpu_registers().r[5] > 5, "it ran");
}

#[test]
fn a_state_taken_inside_an_hle_intr_wait_resumes_identically() {
    let rom = intr_wait_program(b"");
    let mut a = gba(&rom);
    let mut steps = 0;
    while !(a.bus.intr_wait.is_some() && a.bus.halted && a.cpu.retrying()) {
        a.step_instruction();
        steps += 1;
        assert!(steps < 200_000, "never waited");
    }
    let mut b = fork(&a, gba(&rom));
    assert!(b.bus.intr_wait.is_some() && b.bus.halted && b.cpu.retrying());
    let waits = a.cpu_registers().r[5];
    for _ in 0..4 {
        a.step_frame();
        b.step_frame();
    }
    same(&a, &b, "after four frames");
    assert!(a.cpu_registers().r[5] > waits, "the wait returned");
}

#[test]
fn a_state_with_a_filled_prefetch_buffer_resumes_identically() {
    // A block load from IWRAM leaves the cartridge bus idle for eight cycles, so
    // the buffer fills faster than the CPU drains it.
    let loop_at = BASE + 4;
    let p = [
        mov(0, 0x0300_0000),
        LDM_R1_R8,
        LDM_R1_R8,
        b(loop_at + 8, loop_at),
    ];
    let rom = rom(&p, b"");
    let mut a = gba(&rom);
    a.bus.write16(0x0400_0204, 0x4014, N); // WAITCNT: prefetch on, WS0 3/1
    let mut filled = 0;
    for _ in 0..2000 {
        a.step_instruction();
        if a.bus.prefetch.buffered() > 0 {
            filled += 1;
            let mut b = fork(&a, gba(&rom));
            assert_eq!(b.bus.prefetch.buffered(), a.bus.prefetch.buffered());
            step_both(&mut a, &mut b, 50);
            same(&a, &b, "prefetch");
            if filled == 8 {
                break;
            }
        }
    }
    assert!(
        filled >= 8,
        "the prefetch buffer was filled at {filled} points"
    );
}

#[test]
fn a_state_taken_mid_dma_resumes_identically() {
    let rom = rom(&[B_SELF], b"");
    let mut a = gba(&rom);
    for i in 0..200u32 {
        a.bus.write16(0x0200_0000 + 2 * i, (i * 7 + 1) as u16, N);
    }
    dma3(&mut a, 0x0200_0000, 0x0300_0000, 200, 0x8000);
    for _ in 0..60 {
        a.step_instruction();
    }
    let active = a.bus.dma.active_channel();
    assert_eq!(active, Some(3), "still transferring");
    let mut b = fork(&a, gba(&rom));
    assert_eq!(b.bus.dma.active_channel(), Some(3));
    run_dma(&mut a);
    run_dma(&mut b);
    same(&a, &b, "after the DMA");
    for i in 0..200u32 {
        assert_eq!(
            b.peek16(0x0300_0000 + 2 * i),
            (i * 7 + 1) as u16,
            "unit {i}"
        );
    }
}

fn flash_rom() -> Vec<u8> {
    rom(&[B_SELF], b"FLASH1M_V")
}

#[test]
fn a_state_taken_between_flash_command_bytes_completes_the_command() {
    // Program, bank select and sector erase, cut after every write.
    let flash = |addr: u32| 0x0E00_0000 + addr;
    let sequences: [Vec<(u32, u8)>; 3] = [
        vec![
            (0x5555, 0xAA),
            (0x2AAA, 0x55),
            (0x5555, 0xA0),
            (0x0123, 0x42),
        ],
        vec![
            (0x5555, 0xAA),
            (0x2AAA, 0x55),
            (0x5555, 0xB0),
            (0x0000, 1),
            (0x5555, 0xAA),
            (0x2AAA, 0x55),
            (0x5555, 0xA0),
            (0x0010, 0x77),
        ],
        vec![
            (0x5555, 0xAA),
            (0x2AAA, 0x55),
            (0x5555, 0xA0),
            (0x1004, 0x00),
            (0x5555, 0xAA),
            (0x2AAA, 0x55),
            (0x5555, 0x80),
            (0x5555, 0xAA),
            (0x2AAA, 0x55),
            (0x1000, 0x30),
        ],
    ];
    let rom = flash_rom();
    for (s, seq) in sequences.iter().enumerate() {
        for cut in 0..=seq.len() {
            let mut a = gba(&rom);
            for &(addr, v) in &seq[..cut] {
                a.bus.write8(flash(addr), v, N);
            }
            let mut b = fork(&a, gba(&rom));
            for &(addr, v) in &seq[cut..] {
                a.bus.write8(flash(addr), v, N);
                b.bus.write8(flash(addr), v, N);
            }
            same(&a, &b, &format!("sequence {s} cut {cut}"));
            assert_eq!(a.battery_ram(), b.battery_ram());
        }
    }
    // The three sequences really did their work.
    let mut g = gba(&rom);
    for &(addr, v) in &sequences[0] {
        g.bus.write8(flash(addr), v, N);
    }
    assert_eq!(g.battery_ram().unwrap()[0x123], 0x42);
    let mut g = gba(&rom);
    for &(addr, v) in &sequences[1] {
        g.bus.write8(flash(addr), v, N);
    }
    assert_eq!(g.battery_ram().unwrap()[0x1_0010], 0x77, "bank 1");
    let mut g = gba(&rom);
    for &(addr, v) in &sequences[2] {
        g.bus.write8(flash(addr), v, N);
    }
    assert_eq!(g.battery_ram().unwrap()[0x1004], 0xFF, "sector erased");
}

fn put_bits(g: &mut Gba, at: u32, bits: &[u16]) {
    for (i, &bit) in bits.iter().enumerate() {
        g.bus.write16(at + 2 * i as u32, bit, N);
    }
}

fn eeprom_request(read: bool, addr: u32, data: u64) -> Vec<u16> {
    let mut v = vec![1, u16::from(read)];
    v.extend((0..6).rev().map(|i| ((addr >> i) & 1) as u16));
    if !read {
        v.extend((0..64).rev().map(|i| ((data >> i) & 1) as u16));
    }
    v.push(0);
    v
}

#[test]
fn a_state_taken_mid_eeprom_dma_completes_the_transfer() {
    let rom = rom(&[B_SELF], b"EEPROM_V");
    const EEPROM: u32 = 0x0D00_0000;
    const FIXED_DST: u16 = 0x0040;
    const FIXED_SRC: u16 = 0x0100;
    let value = 0x0123_4567_89AB_CDEFu64;

    // A write request: 73 halfwords of IWRAM into the chip, cut part-way.
    let mut a = gba(&rom);
    put_bits(&mut a, 0x0300_0000, &eeprom_request(false, 0x2A, value));
    dma3(&mut a, 0x0300_0000, EEPROM, 73, 0x8000 | FIXED_DST);
    for _ in 0..40 {
        a.step_instruction();
    }
    assert!(a.bus.dma.active_channel().is_some(), "mid-request");
    let mut b = fork(&a, gba(&rom));
    run_dma(&mut a);
    run_dma(&mut b);
    same(&a, &b, "after the write");
    let image = b.battery_ram().expect("the DMA length sized the chip");
    assert_eq!(image.len(), 512);
    assert_eq!(&image[0x2A * 8..0x2A * 8 + 8], &value.to_be_bytes());

    // A read: the 9-halfword request, then 68 halfwords back, cut part-way.
    put_bits(&mut a, 0x0300_0100, &eeprom_request(true, 0x2A, 0));
    put_bits(&mut b, 0x0300_0100, &eeprom_request(true, 0x2A, 0));
    for g in [&mut a, &mut b] {
        dma3(g, 0x0300_0100, EEPROM, 9, 0x8000 | FIXED_DST);
        run_dma(g);
        dma3(g, EEPROM, 0x0300_0200, 68, 0x8000 | FIXED_SRC);
        for _ in 0..30 {
            g.step_instruction();
        }
        assert!(g.bus.dma.active_channel().is_some(), "mid-response");
    }
    let mut c = fork(&a, gba(&rom));
    run_dma(&mut a);
    run_dma(&mut c);
    same(&a, &c, "after the read");
    let bits: Vec<u16> = (0..68).map(|i| c.peek16(0x0300_0200 + 2 * i) & 1).collect();
    assert_eq!(&bits[..4], &[0; 4]);
    let read = bits[4..]
        .iter()
        .fold(0u64, |v, &bit| v << 1 | u64::from(bit));
    assert_eq!(read, value, "the response survived the save");
}

// ---- headers, rollback, corruption -------------------------------------------------

#[test]
fn a_foreign_state_is_refused_and_leaves_the_machine_alone() {
    let rom_a = intr_wait_program(b"FLASH1M_V");
    let mut other_rom = rom_a.clone();
    other_rom[0x300] = 1;
    let bios = |fill: u8| Some(vec![fill; crate::BIOS_SIZE].into_boxed_slice());
    let opts = |save, rtc| CartOptions { save, rtc };
    let mut a = gba(&rom_a);
    for _ in 0..3000 {
        a.step_instruction();
    }
    let snap = a.save_state();
    let before = a.save_state();

    let mut machines: Vec<(&str, Gba, StateError)> = vec![
        (
            "other ROM",
            Gba::new(other_rom, None).unwrap(),
            StateError::RomMismatch,
        ),
        (
            "real BIOS",
            Gba::new(rom_a.clone(), bios(1)).unwrap(),
            StateError::ModelMismatch,
        ),
        (
            "save type",
            Gba::with_options(rom_a.clone(), None, opts(SaveChoice::Sram, RtcMode::Auto)).unwrap(),
            StateError::ModelMismatch,
        ),
        (
            "clock option",
            Gba::with_options(rom_a.clone(), None, opts(SaveChoice::Auto, RtcMode::On)).unwrap(),
            StateError::ModelMismatch,
        ),
    ];
    for (name, g, want) in &mut machines {
        let own = g.save_state();
        assert_eq!(g.load_state(&snap), Err(want.clone()), "{name}");
        assert!(g.save_state() == own, "{name}: unchanged");
    }
    // The other way round, and a different dump of the same kind.
    let mut real = Gba::new(rom_a.clone(), bios(1)).unwrap();
    let real_state = real.save_state();
    assert_eq!(a.load_state(&real_state), Err(StateError::ModelMismatch));
    assert!(a.save_state() == before);
    let mut other_dump = Gba::new(rom_a.clone(), bios(2)).unwrap();
    assert_eq!(
        other_dump.load_state(&real_state),
        Err(StateError::ModelMismatch)
    );
    // The same dump loads, and so does the very same machine.
    let mut same_dump = Gba::new(rom_a.clone(), bios(1)).unwrap();
    same_dump.load_state(&real_state).unwrap();
    real.step_frame();
    real.load_state(&real_state).unwrap();
    assert!(real.save_state() == real_state);
    a.load_state(&snap).unwrap();
}

#[test]
fn bad_headers_are_named() {
    let mut a = gba(&rom(&[B_SELF], b""));
    a.step_frame();
    let snap = a.save_state();
    let mut g = gba(&rom(&[B_SELF], b""));
    assert_eq!(g.load_state(&[]), Err(StateError::BadMagic));
    assert_eq!(g.load_state(b"GBST____"), Err(StateError::BadMagic));
    let mut v = snap.clone();
    v[4] = 9;
    assert_eq!(g.load_state(&v), Err(StateError::UnsupportedVersion(9)));
    let mut v = snap.clone();
    v[6] ^= 1; // ROM hash
    assert_eq!(g.load_state(&v), Err(StateError::RomMismatch));
    for (at, name) in [(14, "BIOS kind"), (24 - 1, "clock option")] {
        let mut v = snap.clone();
        v[at] = 0xEE;
        assert!(
            matches!(g.load_state(&v), Err(StateError::Invalid(_))),
            "{name}"
        );
    }
    let mut v = snap.clone();
    v[15] = 1; // a BIOS hash on the built-in BIOS
    assert!(matches!(g.load_state(&v), Err(StateError::Invalid(_))));
    // A trailing byte, and every truncation of the header.
    let mut v = snap.clone();
    v.push(0);
    assert_eq!(g.load_state(&v), Err(StateError::TrailingBytes));
    for cut in 0..state::HEADER_LEN {
        assert!(g.load_state(&snap[..cut]).is_err(), "cut {cut}");
    }
    g.load_state(&snap).unwrap();
}

/// `(name, start, len)` of each section in `g`'s state, from the components
/// themselves; checked against the real state so the table cannot rot.
fn sections(g: &Gba) -> Vec<(&'static str, usize, usize)> {
    let mut out = Vec::new();
    let mut off = state::HEADER_LEN;
    let mut add = |name, f: &dyn Fn(&mut StateWriter)| {
        let mut w = StateWriter::new();
        w.u8(0); // the tag
        f(&mut w);
        let len = w.into_bytes().len();
        out.push((name, off, len));
        off += len;
    };
    add("cpu", &|w| g.cpu.save_state(w));
    add("scheduler", &|w| g.bus.sched.save_state(w));
    add("bus", &|w| g.bus.save_misc(w));
    add("irq", &|w| g.bus.irq.save_state(w));
    add("timers", &|w| g.bus.timers.save_state(w));
    add("dma", &|w| g.bus.dma.save_state(w));
    add("ppu", &|w| g.bus.ppu.save_state(w));
    add("apu", &|w| g.bus.apu.save_state(w));
    add("keypad", &|w| g.bus.keypad.save_state(w));
    add("prefetch", &|w| g.bus.prefetch.save_state(w));
    add("cart", &|w| g.bus.cart.save_state(w));
    add("rtc start", &|w| save_rtc_start(w, g.rtc_start));
    assert_eq!(
        off,
        g.save_state().len(),
        "the section table covers the state"
    );
    out
}

/// Byte ranges that are bulk data any value of which is valid (work RAM, VRAM,
/// the frames, the save chip), which the corruption tests skip.
fn bulk(g: &Gba, sections: &[(&str, usize, usize)]) -> Vec<std::ops::Range<usize>> {
    let start = |name| sections.iter().find(|s| s.0 == name).unwrap().1;
    let len = |name| sections.iter().find(|s| s.0 == name).unwrap().2;
    let mut v = Vec::new();
    v.push(start("bus") + 1..start("bus") + 1 + 256 * 1024 + 32 * 1024);
    let ppu = start("ppu");
    v.push(ppu + 1 + 0x400..ppu + 1 + 0x400 + 0x1_8000);
    v.push(ppu + len("ppu") - 2 * 76_800..ppu + len("ppu"));
    if let Some(chip) = g.battery_ram() {
        let cart = start("cart");
        v.push(cart + 2..cart + 2 + chip.len().max(8 * 1024));
    }
    v
}

#[test]
fn a_truncated_state_fails_cleanly_at_every_section_boundary() {
    let mut a = gba(&intr_wait_program(b"FLASH_V"));
    for _ in 0..40_000 {
        a.step_instruction();
    }
    let snap = a.save_state();
    let before = a.save_state();
    let mut g = gba(&intr_wait_program(b"FLASH_V"));
    g.load_state(&snap).unwrap();
    for (name, start, len) in sections(&a) {
        for cut in [start, start + 1, start + len / 2, start + len - 1] {
            assert!(g.load_state(&snap[..cut]).is_err(), "{name} cut at {cut}");
            assert!(g.save_state() == before, "{name} cut at {cut}: unchanged");
        }
    }
    assert!(g.load_state(&snap[..snap.len() - 1]).is_err());
    assert!(g.save_state() == before);
    // A wrong section tag is rejected too.
    for (name, start, _) in sections(&a) {
        let mut v = snap.clone();
        v[start] ^= 0x40;
        assert!(g.load_state(&v).is_err(), "{name} tag");
        assert!(g.save_state() == before, "{name} tag: unchanged");
    }
}

/// Every byte of the structured sections set to a few awkward values: the load
/// must be refused with the machine unchanged, or accepted and then run without
/// panicking (a value the emulator can really hold). This is the check that a
/// field that indexes, divides or subtracts is range-checked.
fn corrupt_every_structured_byte(a: &Gba, fresh: Gba, values: &[u8]) {
    let snap = a.save_state();
    let table = sections(a);
    let skip = bulk(a, &table);
    let mut g = fresh;
    g.load_state(&snap).unwrap();
    let before = g.save_state();
    let (mut refused, mut accepted) = (0, 0);
    for (at, &orig) in snap.iter().enumerate() {
        if at < state::HEADER_LEN || skip.iter().any(|r| r.contains(&at)) {
            continue;
        }
        for &v in values {
            if v == orig {
                continue;
            }
            let mut bad = snap.clone();
            bad[at] = v;
            match g.load_state(&bad) {
                Err(_) => {
                    refused += 1;
                    assert!(
                        g.save_state() == before,
                        "refusal at {at} changed the machine"
                    );
                }
                Ok(()) => {
                    accepted += 1;
                    // Reachable by the emulator, so it must be safe to run.
                    g.step_frame();
                    g.drain_audio_into(&mut Vec::new());
                    g.load_state(&snap).unwrap();
                }
            }
        }
    }
    assert!(refused > 100, "the checks refused something ({refused})");
    assert!(accepted > 100, "plain data is accepted ({accepted})");
}

/// The machine `busy_machine` starts from, with the cartridge clock present.
fn busy_fresh(id: &[u8]) -> Gba {
    let options = CartOptions {
        rtc: RtcMode::On,
        ..CartOptions::default()
    };
    Gba::with_options(intr_wait_program(id), None, options).unwrap()
}

/// A machine with every kind of structured state live: all four PSG channels
/// playing (sweep, envelopes, length counters), both sound FIFOs fed by sound DMA
/// and popped by running timers, a cartridge clock halfway through a command, a
/// flash command half written, a large DMA mid-transfer and the HLE wait.
fn busy_machine(id: &[u8]) -> Gba {
    let mut g = busy_fresh(id);
    for i in 0..0x400u32 {
        g.bus.write16(0x0200_0000 + 2 * i, (i * 5) as u16, N);
    }
    let io = |g: &mut Gba, off: u32, v: u16| g.bus.write_io16(off, v, 0xFFFF);
    io(&mut g, 0x204, 0x4317); // WAITCNT: prefetch on, so its fields are live
    io(&mut g, 0x84, 0x80); // sound on
    io(&mut g, 0x80, 0xFF77);
    io(&mut g, 0x82, 0x9B0F | 0x0400); // FIFO A on timer 1, B on timer 0, PSG 100%
    io(&mut g, 0x60, 0x0019); // sweep (down)
    io(&mut g, 0x62, 0xF381);
    io(&mut g, 0x64, 0x8400 | 0x0123);
    io(&mut g, 0x68, 0xA142);
    io(&mut g, 0x6C, 0x84F0);
    io(&mut g, 0x70, 0x00E0);
    io(&mut g, 0x72, 0x6010);
    io(&mut g, 0x74, 0xC500);
    io(&mut g, 0x78, 0xF310);
    io(&mut g, 0x7C, 0x803A);
    for i in 0..3u32 {
        g.bus.write32(0x0400_00A0, 0x0102_0304 * (i + 1), N);
    }
    g.bus.write32(0x0400_00A4, 0x7F80_FF01, N);
    // Sound DMA keeps both FIFOs fed from EWRAM.
    for (base, fifo) in [(0x0400_00BCu32, 0x0400_00A0u32), (0x0400_00C8, 0x0400_00A4)] {
        g.bus.write32(base, 0x0200_0000, N);
        g.bus.write32(base + 4, fifo, N);
        g.bus.write32(base + 8, 4 | 0xB640 << 16, N);
    }
    // Timers 0 and 1 run (with IRQs), 2 counts up from 1.
    for (n, ctl) in [(0u32, 0x00C0u16), (1, 0x00C1), (2, 0x0084)] {
        io(
            &mut g,
            0x100 + 4 * n,
            0xFF00u16.wrapping_add(n as u16 * 0x40),
        );
        io(&mut g, 0x102 + 4 * n, ctl);
    }
    // The RTC: reads on, pins out, CS and SCK up, then three command bits.
    let gpio = |g: &mut Gba, reg: u32, v: u16| g.bus.write16(BASE + reg, v, N);
    gpio(&mut g, 0xC8, 1);
    gpio(&mut g, 0xC6, 7);
    gpio(&mut g, 0xC4, 1);
    gpio(&mut g, 0xC4, 5);
    for bit in [0u16, 1, 1] {
        gpio(&mut g, 0xC4, bit << 1 | 4);
        gpio(&mut g, 0xC4, bit << 1 | 5);
    }
    // Flash: the first two unlock bytes.
    g.bus.write8(0x0E00_5555, 0xAA, N);
    g.bus.write8(0x0E00_2AAA, 0x55, N);
    for _ in 0..30_000 {
        g.step_instruction();
    }
    // A DMA3 mid-transfer, and the audio drained so the APU is at the clock.
    dma3(&mut g, 0x0200_0000, 0x0200_1000, 0x400, 0x8000);
    for _ in 0..100 {
        g.step_instruction();
    }
    assert!(g.bus.dma.active_channel().is_some());
    // The setup took: every channel and the master are on, a FIFO holds data and
    // a timer clocks it.
    assert_eq!(g.bus.apu.read_reg(0x84) & 0x8F, 0x8F, "channels playing");
    assert!(g.bus.apu.fifo_lens().iter().all(|&n| n > 0), "FIFOs fed");
    assert!(g.bus.timers.sound.iter().any(|&s| s), "a FIFO timer runs");
    assert!(g.bus.halted || g.bus.intr_wait.is_some(), "waiting");
    assert!(g.bus.prefetch.enabled);
    g
}

#[test]
fn a_busy_machine_resumes_identically() {
    let mut a = busy_machine(b"FLASH1M_V");
    let mut b = fork(&a, busy_fresh(b"FLASH1M_V"));
    for chunk in 0..40 {
        step_both(&mut a, &mut b, 1000);
        // The mirror the load copies from the APU is the one the bus keeps.
        assert_eq!(
            a.bus.timers.fifo_len,
            a.bus.apu.fifo_lens(),
            "chunk {chunk}"
        );
    }
    same(&a, &b, "busy");
}

#[test]
fn corrupting_structured_fields_never_panics_or_half_loads() {
    let a = busy_machine(b"FLASH_V");
    corrupt_every_structured_byte(&a, busy_fresh(b"FLASH_V"), &[0xFF, 0x80, 0x01]);
}

#[test]
fn corrupting_the_eeprom_and_flash128_sections_never_panics() {
    for id in [&b"EEPROM_V"[..], b"FLASH1M_V"] {
        let a = busy_machine(id);
        corrupt_every_structured_byte(&a, busy_fresh(id), &[0xFF]);
    }
}

#[test]
fn specific_impossible_fields_are_refused() {
    let rom = intr_wait_program(b"FLASH_V");
    let mut a = gba(&rom);
    for _ in 0..30_000 {
        a.step_instruction();
    }
    for _ in 0..20 {
        a.step_frame(); // past the 16 frames an APU lag is allowed
    }
    let snap = a.save_state();
    let table = sections(&a);
    let sec = |name: &str| *table.iter().find(|s| s.0 == name).unwrap();
    let mut g = gba(&rom);
    let mut refuse = |name: &str, bad: &[u8]| {
        let before = g.save_state();
        assert!(
            matches!(g.load_state(bad), Err(StateError::Invalid(_))),
            "{name}"
        );
        assert!(g.save_state() == before, "{name}: unchanged");
    };
    // Flash: bank 1 on a 64 KiB chip would index past the array. The bank byte
    // is the last of the flash fields, before the GPIO block.
    let (cart, cart_len) = (sec("cart").1, sec("cart").2);
    let bank_at = cart + cart_len - a.bus.cart.gpio_state_len() - 1;
    let mut bad = snap.clone();
    bad[bank_at] = 1;
    refuse("flash bank on a 64K chip", &bad);
    // A scheduler with the line end removed: the machine would stall.
    let (sched, _) = (sec("scheduler").1, 0);
    let mut bad = snap.clone();
    bad[sched + 1 + 16] = 0; // 0 entries
    refuse("empty scheduler queue", &bad);
    // PPU: VCOUNT past the frame.
    let ppu = sec("ppu");
    let vcount_at = ppu.1 + ppu.2 - 2 * 76_800 - 1 - 2;
    let mut bad = snap.clone();
    bad[vcount_at..vcount_at + 2].copy_from_slice(&228u16.to_le_bytes());
    refuse("VCOUNT", &bad);
    // APU: a zero frame-sequencer timer would make a zero-length segment.
    // The sequencer timer is followed by cur_l, cur_r, mix_l, mix_r, mix_n and
    // sample_acc: 4 + 4 + 8 + 8 + 4 + 8 bytes.
    let apu = sec("apu");
    let seq_timer_at = apu.1 + apu.2 - (4 + 4 + 4 + 8 + 8 + 4 + 8);
    let mut bad = snap.clone();
    bad[seq_timer_at..seq_timer_at + 4].fill(0);
    refuse("frame sequencer timer", &bad);
    // APU: `now` is ahead of the master clock, or so far behind that the next
    // drain would run the channels for ages.
    let clock = a.cycles();
    assert!(clock > crate::bios::SWI_CYCLE_CAP, "the machine ran");
    for apu_now in [clock + 1, 0] {
        let mut bad = snap.clone();
        bad[apu.1 + 1..apu.1 + 9].copy_from_slice(&apu_now.to_le_bytes());
        refuse("APU clock", &bad);
    }
    // Prefetch: a completion time far ahead would jump the clock by that much
    // (and the machine would then dispatch every event in between).
    let prefetch = sec("prefetch").1;
    let done_at_at = prefetch + 1 + 1 + 1 + 4 + 1;
    let mut bad = snap.clone();
    bad[done_at_at..done_at_at + 8].copy_from_slice(&(clock + (1 << 40)).to_le_bytes());
    refuse("prefetch completion time", &bad);
    // Scheduler: a line end far from the clock would freeze the picture.
    let entries = sched + 1 + 16 + 1;
    let line_end = (0..usize::from(snap[sched + 1 + 16]))
        .map(|i| entries + 18 * i)
        .find(|&e| snap[e] == 3)
        .expect("the line end is queued");
    let mut bad = snap.clone();
    bad[line_end + 2..line_end + 10].copy_from_slice(&(clock + 100_000).to_le_bytes());
    refuse("line end far ahead", &bad);
    let sample_acc_at = apu.1 + apu.2 - 8;
    let mut bad = snap.clone();
    bad[sample_acc_at..sample_acc_at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    refuse("resampler phase", &bad);
}

/// A timer whose `start` does not match its pending overflow event would, on the
/// next timer-register write, replay every period since `start` in one call: a
/// hang for a state taken late in a session (deep-review #2).
#[test]
fn a_timer_start_that_disagrees_with_its_pending_overflow_is_refused() {
    let mut a = busy_machine(b"FLASH_V");
    a.step_frame();
    assert!(a.bus.sched.when(crate::scheduler::Event::TimerOverflow(0)).is_some());
    let mut g = busy_fresh(b"FLASH_V");
    g.load_state(&a.save_state()).expect("the machine itself loads");
    a.bus.timers.t[0].reload = 0xFFFF;
    a.bus.timers.t[0].counter = 0;
    a.bus.timers.t[0].start = 0;
    let before = g.save_state();
    assert_eq!(
        g.load_state(&a.save_state()),
        Err(StateError::Invalid(
            "timer overflow event disagrees with the timer"
        ))
    );
    assert!(g.save_state() == before, "unchanged");
}

#[test]
fn loading_drops_the_buffered_audio_and_marks_the_save_changed() {
    let rom = flash_rom();
    let mut a = gba(&rom);
    a.step_frame();
    a.drain_audio_into(&mut Vec::new()); // bring the APU up to the clock
    let snap = a.save_state();
    a.step_frame();
    let generation = a.battery_generation();
    a.load_state(&snap).unwrap();
    let mut audio = Vec::new();
    a.drain_audio_into(&mut audio);
    // Only what the time since the load produced (none: no time has passed).
    assert!(audio.is_empty(), "{} samples survived", audio.len());
    assert_eq!(a.battery_generation(), generation + 1);
    // A refused load leaves both alone.
    a.step_frame();
    let generation = a.battery_generation();
    let pending = {
        let mut copy = Vec::new();
        a.bus.apu.drain_samples(&mut copy);
        copy.len()
    };
    assert!(pending > 0, "a frame of audio");
    assert!(a.load_state(&snap[..100]).is_err());
    assert_eq!(a.battery_generation(), generation);
    // The sample rate is the host's, not the state's.
    a.set_audio_sample_rate(44_100);
    a.load_state(&snap).unwrap();
    assert_eq!(a.audio_sample_rate(), 44_100);
}

#[test]
fn the_fifo_length_mirror_follows_the_apu_after_a_load() {
    let rom = flash_rom();
    let mut a = gba(&rom);
    // Fill FIFO A with two words and FIFO B with one through the bus.
    a.bus.write32(0x0400_00A0, 0x0403_0201, N);
    a.bus.write32(0x0400_00A0, 0x0807_0605, N);
    a.bus.write32(0x0400_00A4, 0x0C0B_0A09, N);
    assert_eq!(a.bus.timers.fifo_len, [8, 4]);
    assert_eq!(a.bus.apu.fifo_lens(), [8, 4]);
    let b = fork(&a, gba(&rom));
    assert_eq!(b.bus.timers.fifo_len, [8, 4]);
}

/// The state's size without the scheduler queue, whose length varies with the
/// events pending (18 bytes each, up to every variant: 14).
fn fixed_size(g: &Gba) -> usize {
    g.save_state().len() - 18 * g.bus.sched.pending()
}

#[test]
fn the_state_size_is_fixed_per_chip_and_fits_the_cap() {
    let mut sizes = Vec::new();
    for id in [&b""[..], b"SRAM_V", b"FLASH_V", b"FLASH1M_V", b"EEPROM_V"] {
        let mut g = gba(&intr_wait_program(id));
        let first = fixed_size(&g);
        for _ in 0..3 {
            for _ in 0..30_000 {
                g.step_instruction();
            }
            assert_eq!(fixed_size(&g), first, "constant for {id:?}");
        }
        sizes.push(first);
    }
    // The worst case: 128 KiB flash and a full queue.
    let worst = *sizes.iter().max().unwrap() + 18 * 14;
    assert_eq!(
        worst,
        sizes[3] + 18 * 14,
        "128 KiB flash is the worst case: {sizes:?}"
    );
    assert!(
        worst <= crate::MAX_STATE_BYTES,
        "{worst} bytes exceed the cap {}",
        crate::MAX_STATE_BYTES
    );
    assert!(
        crate::MAX_STATE_BYTES - worst <= 16 * 1024,
        "the cap {} is more than a little above the worst case {worst}",
        crate::MAX_STATE_BYTES
    );
    // A real BIOS and a set clock change nothing about the size.
    let mut g = Gba::new(
        intr_wait_program(b"FLASH1M_V"),
        Some(vec![0; crate::BIOS_SIZE].into_boxed_slice()),
    )
    .unwrap();
    g.set_rtc(RtcTime::new(2024, 2, 29, 13, 45, 9).unwrap());
    assert_eq!(fixed_size(&g), sizes[3]);
}

/// code-review ST#1: a failed load must not depend on this machine's own state
/// passing the loader's validators (the old rollback re-loaded a snapshot of it
/// and `expect`ed that to succeed). Here the live machine holds a DMA control
/// value the loader would refuse; a bad load leaves it exactly as it was, and a
/// good state still loads afterwards.
#[test]
fn a_failed_load_leaves_a_machine_the_validators_would_refuse_untouched() {
    let rom = flash_rom();
    let mut good = gba(&rom);
    good.step_frame();
    let snap = good.save_state();

    let mut g = gba(&rom);
    g.step_frame();
    g.step_frame();
    g.bus.dma.ch[0].control = 0x0001; // bits no write can store
    g.bus.irq.if_ = 0x8000; // nor can an IF bit past the 14 sources
    let before = g.save_state();
    assert!(
        gba(&rom).load_state(&before).is_err(),
        "this machine's own state is one the loader refuses"
    );
    for bad in [&snap[..snap.len() / 2], &snap[..30], b"GBAS"] {
        assert!(g.load_state(bad).is_err());
        assert!(g.save_state() == before, "unchanged after a failed load");
    }
    let mut corrupt = snap.clone();
    let apu = sections(&good).iter().find(|s| s.0 == "apu").unwrap().1;
    corrupt[apu + 1 + 8 + 0x20 * 2 + 1] = 7; // a bool that is not 0 or 1
    assert!(g.load_state(&corrupt).is_err());
    assert!(g.save_state() == before);
    // The machine still runs, and a good state loads and round-trips.
    g.step_frame();
    g.load_state(&snap).unwrap();
    assert!(g.save_state() == snap);
    let again = g.save_state();
    g.step_frame();
    g.load_state(&again).unwrap();
    assert!(g.save_state() == again);
}

/// The Thumb decode masks its table index, so even a state whose pipeline holds
/// ARM opcodes in Thumb state loads and runs without panicking.
#[test]
fn a_thumb_state_with_arm_opcodes_in_the_pipeline_runs() {
    let rom = flash_rom();
    let mut a = gba(&rom);
    a.step_frame();
    let snap = a.save_state();
    let cpu = state::HEADER_LEN + 1;
    let mut bad = snap.clone();
    let cpsr_at = cpu + 4 * 16;
    let cpsr = u32::from_le_bytes(bad[cpsr_at..cpsr_at + 4].try_into().unwrap()) | 0x20;
    bad[cpsr_at..cpsr_at + 4].copy_from_slice(&cpsr.to_le_bytes());
    let pipe_at = cpsr_at + 4 + 4 * 6 + 4 * 12 + 4 * 5 + 4 * 5;
    bad[pipe_at..pipe_at + 8].fill(0xFF);
    let mut g = gba(&rom);
    g.load_state(&bad).unwrap();
    g.step_frame();
}

#[test]
fn the_clock_start_is_part_of_the_state() {
    // code-review ST#8: a loaded machine reports the start its state came from,
    // so a recording header made after a load matches the machine.
    let rom = flash_rom();
    let t = RtcTime::new(2024, 2, 29, 13, 45, 9).unwrap();
    let mut with = gba(&rom);
    with.set_rtc(t);
    let mut without = gba(&rom);
    assert_eq!((with.rtc_start(), without.rtc_start()), (Some(t), None));
    let (s_with, s_without) = (with.save_state(), without.save_state());
    without.load_state(&s_with).unwrap();
    assert_eq!(without.rtc_start(), Some(t));
    with.load_state(&s_without).unwrap();
    assert_eq!(with.rtc_start(), None);
    // A flag without a time, or a time that is not a date, is refused.
    let tail = s_with.len() - 8;
    let mut bad = s_with.clone();
    bad[tail + 3] = 31; // day... of month 2
    assert!(gba(&rom).load_state(&bad).is_err());
    let mut bad = s_without.clone();
    bad[tail] = 5;
    assert!(gba(&rom).load_state(&bad).is_err());
}
