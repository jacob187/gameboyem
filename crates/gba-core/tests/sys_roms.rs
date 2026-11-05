//! jsmolka/gba-tests ROMs for the system side: memory, save types. Each runs until
//! the PC reaches the idle `b .` and checks r12 == 0 (gba-facts.md §1a). They need
//! the real CPU, so they are `#[ignore]`d on the system branch and skipped when the
//! ROMs are not checked out. Run with `cargo test -p gba-core --test sys_roms --
//! --ignored`.

use gba_core::Gba;
use std::path::Path;

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../roms/test/gba/jsmolka");

/// Run `rel` until PC == `idle` (a `b .` the ROM ends in) and return r12.
fn run_rom(rel: &str, idle: u32) -> Option<u32> {
    let path = Path::new(ROOT).join(rel);
    let rom = std::fs::read(&path).ok()?;
    let mut gba = Gba::new(rom, None).unwrap();
    // A few frames is plenty: the tests are tiny (flash erases are instant).
    for _ in 0..30 {
        gba.step_frame();
        if gba.cpu_registers().r[15] == idle {
            return Some(gba.cpu_registers().r[12]);
        }
    }
    panic!("{rel}: never reached the idle loop at {idle:#x}");
}

fn check(rel: &str, idle: u32) {
    match run_rom(rel, idle) {
        None => eprintln!("skipping {rel}: ROM not found"),
        Some(r12) => assert_eq!(r12, 0, "{rel}: failed test {r12}"),
    }
}

#[test]
#[ignore = "needs P1-CPU"]
fn memory() {
    check("memory/memory.gba", 0x0800_04C8);
}

#[test]
#[ignore = "needs P1-CPU"]
fn save_sram() {
    check("save/sram.gba", 0x0800_0470);
}

#[test]
#[ignore = "needs P1-CPU"]
fn save_flash64() {
    check("save/flash64.gba", 0x0800_0AC8);
}

#[test]
#[ignore = "needs P1-CPU"]
fn save_flash128() {
    check("save/flash128.gba", 0x0800_0C4C);
}

#[test]
#[ignore = "needs P1-CPU"]
fn save_none() {
    check("save/none.gba", 0x0800_02A8);
}
