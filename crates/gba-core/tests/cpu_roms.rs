//! CPU conformance ROMs (jsmolka's arm.gba/thumb.gba and DenSinH's FuzzARM) run
//! on the bare CPU with a flat-memory bus: no PPU, DMA or timers. The only
//! hardware the ROMs poll is DISPSTAT's VBlank flag (jsmolka's `m_vsync`), which
//! this bus derives from the cycle count. ROMs that are missing skip, unless
//! `CI` is set. `ROM_TEST_SPEED=1` prints instructions per second.

use gba_core::bus::{Access, CpuBus};
use gba_core::cpu::Cpu;
use std::path::PathBuf;

const LINE_CYCLES: u64 = 1232;

/// Flat memory with the GBA address map's mirrors and a derived VBlank flag.
struct FlatBus {
    bios: Vec<u8>,
    ewram: Vec<u8>,
    iwram: Vec<u8>,
    io: Vec<u8>,
    palette: Vec<u8>,
    vram: Vec<u8>,
    oam: Vec<u8>,
    rom: Vec<u8>,
    cycles: u64,
}

impl FlatBus {
    fn new(rom: Vec<u8>) -> Self {
        Self {
            bios: vec![0; 0x4000],
            ewram: vec![0; 0x4_0000],
            iwram: vec![0; 0x8000],
            io: vec![0; 0x400],
            palette: vec![0; 0x400],
            vram: vec![0; 0x2_0000],
            oam: vec![0; 0x400],
            rom,
            cycles: 0,
        }
    }

    fn vcount(&self) -> u16 {
        ((self.cycles / LINE_CYCLES) % 228) as u16
    }

    fn io16(&self, off: usize) -> u16 {
        match off {
            // DISPSTAT: VBlank flag (bit 0) from the line counter.
            0x004 => u16::from(self.vcount() >= 160),
            0x006 => self.vcount(),
            // KEYINPUT: nothing pressed.
            0x130 => 0x03FF,
            _ => u16::from_le_bytes([self.io[off], self.io[off + 1]]),
        }
    }

    fn rd8(&self, addr: u32) -> u8 {
        let a = addr as usize;
        match addr >> 24 {
            0x00 if a < 0x4000 => self.bios[a],
            0x02 => self.ewram[a & 0x3_FFFF],
            0x03 => self.iwram[a & 0x7FFF],
            0x04 => {
                let off = a & 0x3FF;
                (self.io16(off & !1) >> ((off & 1) * 8)) as u8
            }
            0x05 => self.palette[a & 0x3FF],
            0x06 => self.vram[a & 0x1_FFFF],
            0x07 => self.oam[a & 0x3FF],
            0x08..=0x0D => self.rom.get(a & 0x01FF_FFFF).copied().unwrap_or(0),
            _ => 0,
        }
    }

    fn wr8(&mut self, addr: u32, v: u8) {
        let a = addr as usize;
        match addr >> 24 {
            0x02 => self.ewram[a & 0x3_FFFF] = v,
            0x03 => self.iwram[a & 0x7FFF] = v,
            0x04 => self.io[a & 0x3FF] = v,
            0x05 => self.palette[a & 0x3FF] = v,
            0x06 => self.vram[a & 0x1_FFFF] = v,
            0x07 => self.oam[a & 0x3FF] = v,
            _ => {}
        }
    }

    fn rd16(&self, addr: u32) -> u16 {
        let a = addr & !1;
        u16::from_le_bytes([self.rd8(a), self.rd8(a + 1)])
    }

    fn rd32(&self, addr: u32) -> u32 {
        let a = addr & !3;
        u32::from(self.rd16(a)) | (u32::from(self.rd16(a + 2)) << 16)
    }
}

impl CpuBus for FlatBus {
    fn read8(&mut self, addr: u32, _a: Access) -> u8 {
        self.cycles += 1;
        self.rd8(addr)
    }
    fn read16(&mut self, addr: u32, _a: Access) -> u16 {
        self.cycles += 1;
        self.rd16(addr)
    }
    fn read32(&mut self, addr: u32, _a: Access) -> u32 {
        self.cycles += 1;
        self.rd32(addr)
    }
    fn write8(&mut self, addr: u32, v: u8, _a: Access) {
        self.cycles += 1;
        self.wr8(addr, v);
    }
    fn write16(&mut self, addr: u32, v: u16, _a: Access) {
        self.cycles += 1;
        let a = addr & !1;
        for (i, b) in v.to_le_bytes().into_iter().enumerate() {
            self.wr8(a + i as u32, b);
        }
    }
    fn write32(&mut self, addr: u32, v: u32, _a: Access) {
        self.cycles += 1;
        let a = addr & !3;
        for (i, b) in v.to_le_bytes().into_iter().enumerate() {
            self.wr8(a + i as u32, b);
        }
    }
    fn idle(&mut self, cycles: u32) {
        self.cycles += u64::from(cycles);
    }
    fn irq_line(&self) -> bool {
        false
    }
}

fn rom_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../roms/test/gba")
        .join(rel)
}

/// Load a test ROM, or `None` (skip) if it is absent and not in CI.
fn load(rel: &str) -> Option<Vec<u8>> {
    let p = rom_path(rel);
    match std::fs::read(&p) {
        Ok(d) => Some(d),
        Err(_) => {
            assert!(std::env::var_os("CI").is_none(), "{rel} missing in CI");
            eprintln!("skip {rel}: {} not found", p.display());
            None
        }
    }
}

struct RunEnd {
    pc: u32,
    steps: u64,
    r: [u32; 16],
}

/// Step until `stop(pc)` or `max_steps`; the final state is returned.
fn run(rom: Vec<u8>, max_steps: u64, stop: impl Fn(u32) -> bool) -> RunEnd {
    let mut bus = FlatBus::new(rom);
    let mut cpu = Cpu::post_bios();
    let started = std::time::Instant::now();
    let mut steps = 0;
    let mut pc = cpu.registers().r[15];
    while steps < max_steps && !stop(pc) {
        cpu.step(&mut bus);
        steps += 1;
        pc = cpu.registers().r[15];
    }
    if std::env::var_os("ROM_TEST_SPEED").is_some() {
        let s = started.elapsed().as_secs_f64();
        eprintln!(
            "{steps} instructions in {s:.3} s = {:.1} M/s ({} cycles)",
            steps as f64 / s / 1e6,
            bus.cycles
        );
    }
    RunEnd {
        pc,
        steps,
        r: cpu.registers().r,
    }
}

/// jsmolka's tests end in `idle: b idle`; the result is r12 (r7 for Thumb), 0 on
/// pass. A failing test calls SWI 6 to print its number, which lands in the
/// (empty) BIOS region, so an exit there is also a failure.
fn jsmolka(rel: &str, idle_addr: u32, thumb_idle: bool, result_reg: usize) {
    let Some(rom) = load(rel) else { return };
    let off = (idle_addr & 0x01FF_FFFF) as usize;
    if thumb_idle {
        assert_eq!(
            &rom[off..off + 2],
            &[0xFE, 0xE7],
            "no `b .` at {idle_addr:#X}"
        );
    } else {
        assert_eq!(
            &rom[off..off + 4],
            &[0xFE, 0xFF, 0xFF, 0xEA],
            "no `b .` at {idle_addr:#X}"
        );
    }
    let end = run(rom, 50_000_000, |pc| pc == idle_addr || pc < 0x4000);
    assert_eq!(
        end.pc, idle_addr,
        "{rel} left the program (pc {:#X})",
        end.pc
    );
    assert_eq!(
        end.r[result_reg], 0,
        "{rel}: failed test number {} (after {} instructions)",
        end.r[result_reg], end.steps
    );
}

#[test]
fn jsmolka_arm() {
    jsmolka("jsmolka/arm/arm.gba", 0x0800_1EC4, false, 12);
}

#[test]
fn jsmolka_thumb() {
    // The Thumb tests return to ARM code for the shared `eval`/`idle` tail.
    jsmolka("jsmolka/thumb/thumb.gba", 0x0800_0AAC, false, 7);
}

/// FuzzARM: EWRAM word 0 stays 0 unless a test failed ('AAAA'/'TTTT' dump). The
/// ROM must also run to its end-of-testing loop, so we require the PC to sit in
/// the same small loop for the last 100k steps with word 0 still zero.
fn fuzzarm(name: &str) {
    let Some(rom) = load(&format!("fuzzarm/{name}")) else {
        return;
    };
    let mut bus = FlatBus::new(rom);
    let mut cpu = Cpu::post_bios();
    let started = std::time::Instant::now();
    let mut steps = 0u64;
    let mut stuck = 0u32;
    let mut last = cpu.registers().r[15];
    while steps < 200_000_000 {
        cpu.step(&mut bus);
        steps += 1;
        let pc = cpu.registers().r[15];
        if pc == last {
            stuck += 1;
            if stuck > 1000 {
                break;
            }
        } else {
            stuck = 0;
        }
        last = pc;
        // A failing test writes its record to EWRAM; stop early.
        if steps % 4096 == 0 && bus.rd32(0x0200_0000) != 0 {
            break;
        }
    }
    if std::env::var_os("ROM_TEST_SPEED").is_some() {
        let secs = started.elapsed().as_secs_f64();
        eprintln!(
            "{name}: {steps} instructions in {secs:.3} s = {:.1} M/s",
            steps as f64 / secs / 1e6
        );
    }
    let word0 = bus.rd32(0x0200_0000);
    let dump: Vec<u32> = (0..16).map(|i| bus.rd32(0x0200_0000 + i * 4)).collect();
    assert_eq!(
        word0, 0,
        "{name}: failure record after {steps} instructions (pc {last:#X}): {dump:08X?}"
    );
    assert!(
        stuck > 1000,
        "{name}: never reached an idle loop in {steps} instructions (pc {last:#X})"
    );
}

#[test]
fn fuzzarm_arm_data_processing() {
    fuzzarm("ARM_DataProcessing.gba");
}

#[test]
fn fuzzarm_thumb_data_processing() {
    fuzzarm("THUMB_DataProcessing.gba");
}

#[test]
fn fuzzarm_arm_any() {
    fuzzarm("ARM_Any.gba");
}

#[test]
fn fuzzarm_thumb_any() {
    fuzzarm("THUMB_Any.gba");
}

#[test]
fn fuzzarm_mixed() {
    fuzzarm("FuzzARM.gba");
}
