//! The save window (0x0E00_0000) through the bus, mirroring jsmolka's sram.gba /
//! flash.gba / none.gba tests (t001..t012), plus the facade's battery API.

mod sys_common;
use gba_core::bus::{Bus, CpuBus};
use gba_core::{CartError, Gba};
use sys_common::*;

const SAVE: u32 = 0x0E00_0000;

fn flash_cmd(b: &mut Bus, cmd: u8) {
    b.write8(SAVE + 0x5555, 0xAA, N);
    b.write8(SAVE + 0x2AAA, 0x55, N);
    b.write8(SAVE + 0x5555, cmd, N);
}

fn flash_program(b: &mut Bus, addr: u32, v: u8) {
    flash_cmd(b, 0xA0);
    b.write8(addr, v, N);
}

#[test]
fn sram_behaves_like_jsmolka_expects() {
    let mut b = bus_with(rom_with(b"SRAM_V113"));
    let mut a = SAVE;
    // t001: uninitialised reads 0xFF.
    assert_eq!(b.read8(a, N), 0xFF);
    // t002/t003: mirrors every 32 KiB, in both 0x0E and 0x0F.
    a += 32;
    b.write8(a, 1, N);
    assert_eq!(b.read8(a + 0x1_0000, N), 1);
    a += 32;
    b.write8(a, 1, N);
    assert_eq!(b.read8(a + 0x0100_0000, N), 1);
    // t004/t005: 16/32-bit loads see the byte repeated.
    a += 32;
    b.write8(a, 1, N);
    assert_eq!(b.read16(a, N), 0x0101);
    a += 32;
    b.write8(a, 1, N);
    assert_eq!(b.read32(a, N), 0x0101_0101);
    // t006: a halfword store writes the lane selected by the address, at the
    // unaligned address.
    a += 32;
    b.write16(a, 0xAABB, N);
    assert_eq!(b.read8(a, N), 0xBB);
    b.write16(a + 1, 0xAABB, N);
    assert_eq!(b.read8(a + 1, N), 0xAA);
    // t007: ... and only that one byte.
    a += 32;
    b.write16(a, 0xAABB, N);
    assert_eq!(b.read8(a, N), 0xBB);
    assert_eq!(b.read8(a + 1, N), 0xFF);
    // t008: word stores likewise, one lane per address.
    a += 32;
    for (i, want) in [0xDD, 0xCC, 0xBB, 0xAA].into_iter().enumerate() {
        b.write32(a + i as u32, 0xAABB_CCDD, N);
        assert_eq!(b.read8(a + i as u32, N), want);
    }
    // t009: a word store at an aligned address writes just that byte.
    a += 32;
    b.write32(a, 0xAABB_CCDD, N);
    assert_eq!(b.read8(a, N), 0xDD);
    assert_eq!([1, 2, 3].map(|i| b.read8(a + i, N)), [0xFF; 3]);
}

#[test]
fn sram_access_costs_follow_waitcnt() {
    let mut b = bus_with(rom_with(b"SRAM_V113"));
    assert_eq!(cycles(&mut b, |b| b.read8(SAVE, N)), 5);
    assert_eq!(cycles(&mut b, |b| b.write8(SAVE, 1, S)), 5);
    b.write16(0x0400_0204, 3, N);
    assert_eq!(cycles(&mut b, |b| b.read8(SAVE, N)), 9);
}

#[test]
fn no_save_chip_reads_ff() {
    let mut b = bus();
    assert_eq!(b.read8(SAVE, N), 0xFF);
    assert_eq!(b.read8(SAVE + 0x0100_0000, N), 0xFF);
    b.write8(SAVE, 1, N);
    assert_eq!(b.read8(SAVE, N), 0xFF);
    assert_eq!(b.read32(SAVE, N), 0xFFFF_FFFF);
}

#[test]
fn flash_programs_after_the_command_and_mirrors() {
    let mut b = bus_with(rom_with(b"FLASH_V126"));
    assert_eq!(b.read8(SAVE, N), 0xFF);
    // Without the command a store does nothing.
    b.write8(SAVE + 0x20, 1, N);
    assert_eq!(b.read8(SAVE + 0x20, N), 0xFF);
    // t002/t003 mirrors: 64 KiB window, and 0x0F.
    flash_program(&mut b, SAVE + 0x40, 1);
    assert_eq!(b.read8(SAVE + 0x40 + 0x1_0000, N), 1);
    assert_eq!(b.read8(SAVE + 0x40 + 0x0100_0000, N), 1);
    // t004: loads repeat the byte.
    assert_eq!(b.read16(SAVE + 0x40, N), 0x0101);
    // t006/t008: the lane lands at the unaligned address; each store needs its
    // own command.
    flash_cmd(&mut b, 0xA0);
    b.write32(SAVE + 0x81, 0xAABB_CCDD, N);
    assert_eq!(b.read8(SAVE + 0x81, N), 0xCC);
    assert_eq!(b.read8(SAVE + 0x80, N), 0xFF);
    flash_cmd(&mut b, 0xA0);
    b.write16(SAVE + 0xA1, 0xAABB, N);
    assert_eq!(b.read8(SAVE + 0xA1, N), 0xAA);
}

#[test]
fn flash_erase_chip_and_sector() {
    let mut b = bus_with(rom_with(b"FLASH_V126"));
    for i in 0..0x2000 {
        flash_program(&mut b, SAVE + i, 0);
    }
    // t011: erase the first 4 KiB sector only.
    flash_cmd(&mut b, 0x80);
    b.write8(SAVE + 0x5555, 0xAA, N);
    b.write8(SAVE + 0x2AAA, 0x55, N);
    b.write8(SAVE, 0x30, N);
    assert_eq!(b.read8(SAVE, N), 0xFF);
    assert_eq!(b.read8(SAVE + 0xFFF, N), 0xFF);
    assert_eq!(b.read8(SAVE + 0x1000, N), 0);
    // t010: erase the chip.
    flash_cmd(&mut b, 0x80);
    flash_cmd(&mut b, 0x10);
    assert_eq!(b.read8(SAVE + 0x1000, N), 0xFF);
}

#[test]
fn flash_id_and_bank_switch() {
    let mut b = bus_with(rom_with(b"FLASH1M_V103"));
    flash_cmd(&mut b, 0x90);
    assert_eq!((b.read8(SAVE, N), b.read8(SAVE + 1, N)), (0x62, 0x13));
    flash_cmd(&mut b, 0xF0);
    assert_eq!(b.read8(SAVE, N), 0xFF);
    // t012: write in bank 0, switch, bank 1 is separate.
    flash_program(&mut b, SAVE + 0x10, 0x11);
    flash_cmd(&mut b, 0xB0);
    b.write8(SAVE, 1, N);
    assert_eq!(b.read8(SAVE + 0x10, N), 0xFF);
    flash_program(&mut b, SAVE + 0x10, 0x22);
    flash_cmd(&mut b, 0xB0);
    b.write8(SAVE, 0, N);
    assert_eq!(b.read8(SAVE + 0x10, N), 0x11);
    let img = b.cart.battery_ram().unwrap();
    assert_eq!((img.len(), img[0x10], img[0x1_0010]), (131_072, 0x11, 0x22));
}

fn gba_with(id: &[u8]) -> Gba {
    Gba::new(rom_with(id), None).unwrap()
}

#[test]
fn facade_battery_api() {
    let mut g = gba_with(b"SRAM_V113");
    assert_eq!(g.battery_ram().unwrap().len(), 32 * 1024);
    assert_eq!(g.battery_generation(), 0);
    let mut img = vec![0xFF; 32 * 1024];
    img[7] = 0x42;
    g.load_battery_ram(&img).unwrap();
    assert_eq!(g.battery_ram().unwrap()[7], 0x42);
    assert_eq!(g.battery_generation(), 1);
    assert!(matches!(
        g.load_battery_ram(&[0; 5]),
        Err(CartError::SaveSize { got: 5, .. })
    ));
    assert_eq!(
        g.battery_ram().unwrap()[7],
        0x42,
        "a rejected load changes nothing"
    );
    assert_eq!(g.battery_generation(), 1);

    let mut none = gba_with(&[]);
    assert!(none.battery_ram().is_none());
    assert_eq!(none.load_battery_ram(&[0; 5]), Err(CartError::NoSave));

    let mut ee = gba_with(b"EEPROM_V124");
    assert!(ee.battery_ram().is_none());
    ee.load_battery_ram(&[0xFF; 8192]).unwrap();
    assert_eq!(ee.battery_ram().unwrap().len(), 8192);
    assert!(ee.load_battery_ram(&vec![0; 100_000]).is_err());
}

#[test]
fn peeks_reach_every_region() {
    let g = gba_with(&[]);
    assert_eq!(g.peek32(0x0800_0000), 0);
    assert_eq!(g.peek16(0x0800_0200), 0x0100);
    assert_eq!(g.peek8(0x0E00_0000), 0xFF);
}
