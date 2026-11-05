//! Memory map, wait states, open bus and I/O register behaviour, driven through
//! `CpuBus` directly (the CPU is a stub on this branch).

mod sys_common;
use gba_core::bus::CpuBus;
use gba_core::irq::Interrupt;
use gba_core::keypad::Button;
use sys_common::*;

const WAITCNT: u32 = 0x0400_0204;

#[test]
fn internal_ram_costs_and_mirrors() {
    let mut b = bus();
    assert_eq!(
        cycles(&mut b, |b| b.write32(0x0200_0000, 0xDEAD_BEEF, N)),
        6
    );
    assert_eq!(cycles(&mut b, |b| b.read16(0x0200_0002, N)), 3);
    assert_eq!(cycles(&mut b, |b| b.read8(0x0200_0000, S)), 3);
    assert_eq!(
        b.read32(0x0204_0000, N),
        0xDEAD_BEEF,
        "EWRAM mirrors every 256 KiB"
    );
    assert_eq!(cycles(&mut b, |b| b.write32(0x0300_0000, 1, N)), 1);
    assert_eq!(cycles(&mut b, |b| b.read32(0x0300_0000, N)), 1);
    assert_eq!(b.read32(0x0300_8000, N), 1, "IWRAM mirrors every 32 KiB");
    assert_eq!(b.read32(0x03FF_8000, N), 1);
    // Little endian, aligned-down halves.
    b.write32(0x0300_0010, 0x1122_3344, N);
    assert_eq!(b.read16(0x0300_0012, N), 0x1122);
    assert_eq!(b.read16(0x0300_0013, N), 0x1122, "address is aligned down");
    assert_eq!(b.read8(0x0300_0011, N), 0x33);
}

#[test]
fn video_memory_costs_and_mirrors() {
    let mut b = bus();
    assert_eq!(cycles(&mut b, |b| b.write16(0x0500_0010, 0x1234, N)), 1);
    assert_eq!(cycles(&mut b, |b| b.write32(0x0500_0010, 0x1234, N)), 2);
    assert_eq!(cycles(&mut b, |b| b.read32(0x0600_0000, N)), 2);
    assert_eq!(cycles(&mut b, |b| b.read8(0x0700_0000, N)), 1);
    assert_eq!(
        cycles(&mut b, |b| b.write32(0x0700_0000, 1, N)),
        1,
        "OAM is 32-bit"
    );
    b.write32(0x0500_0010, 1, N);
    assert_eq!(b.read32(0x0500_0410, N), 1, "palette mirrors every 1 KiB");
    b.write32(0x0600_0000, 2, N);
    assert_eq!(b.read32(0x0602_0000, N), 2, "VRAM mirrors every 128 KiB");
    b.write32(0x0700_0000, 3, N);
    assert_eq!(b.read32(0x0700_0400, N), 3, "OAM mirrors every 1 KiB");
    // The OBJ-area mirror (jsmolka memory.gba t005, tile mode).
    b.write16(0x0400_0000, 0, N);
    b.write32(0x0601_0000, 4, N);
    assert_eq!(b.read32(0x0601_8000, N), 4);
}

#[test]
fn byte_write_rules() {
    let mut b = bus();
    // Palette: duplicated.
    b.write8(0x0500_0001, 0x55, N);
    assert_eq!(b.read16(0x0500_0000, N), 0x5555);
    // OAM: ignored.
    b.write32(0x0700_0000, 0x1234_5678, N);
    b.write8(0x0700_0000, 0xFF, N);
    assert_eq!(b.read32(0x0700_0000, N), 0x1234_5678);
    // Tile mode: BG VRAM (below 0x1_0000) duplicates, OBJ VRAM ignores.
    b.write16(0x0400_0000, 0, N);
    b.write8(0x0600_0000, 2, N);
    assert_eq!(b.read16(0x0600_0000, N), 0x0202);
    b.write8(0x0601_0000, 2, N);
    assert_eq!(b.read16(0x0601_0000, N), 0);
    // 0x1_0000..0x1_4000 is BG in a bitmap mode (3), OBJ from 0x1_4000.
    b.write16(0x0400_0000, 3, N);
    b.write8(0x0601_0000, 3, N);
    assert_eq!(b.read16(0x0601_0000, N), 0x0303);
    b.write8(0x0601_3FFF, 4, N);
    assert_eq!(b.read16(0x0601_3FFE, N), 0x0404);
    b.write8(0x0601_4000, 5, N);
    assert_eq!(b.read16(0x0601_4000, N), 0);
    // Mode 5 is a bitmap mode too; mode 2 is not.
    b.write16(0x0400_0000, 5, N);
    b.write8(0x0601_2000, 6, N);
    assert_eq!(b.read16(0x0601_2000, N), 0x0606);
    b.write16(0x0400_0000, 2, N);
    b.write8(0x0601_2000, 7, N);
    assert_eq!(b.read16(0x0601_2000, N), 0x0606, "now OBJ VRAM: ignored");
    // Halfword/word writes are never affected.
    b.write16(0x0601_0000, 0xABCD, N);
    assert_eq!(b.read16(0x0601_0000, N), 0xABCD);
}

#[test]
fn rom_wait_states_follow_waitcnt() {
    let mut b = bus();
    // Reset: WS0 4,2 -> 5 and 3 cycles per halfword.
    assert_eq!(cycles(&mut b, |b| b.read16(0x0800_0000, N)), 5);
    assert_eq!(cycles(&mut b, |b| b.read16(0x0800_0002, S)), 3);
    assert_eq!(cycles(&mut b, |b| b.read32(0x0800_0004, N)), 8);
    assert_eq!(cycles(&mut b, |b| b.read32(0x0800_0008, S)), 6);
    assert_eq!(cycles(&mut b, |b| b.read8(0x0800_0008, N)), 5);
    // WS1 reset: N 4, S 4; WS2: N 4, S 8.
    assert_eq!(cycles(&mut b, |b| b.read16(0x0A00_0002, S)), 5);
    assert_eq!(cycles(&mut b, |b| b.read16(0x0C00_0002, S)), 9);
    // The three areas are mirrors of each other.
    let mut rom = rom_with(&[]);
    rom[0x10] = 0x77;
    let mut b = bus_with(rom);
    assert_eq!(b.read8(0x0800_0010, N), 0x77);
    assert_eq!(b.read8(0x0A00_0010, N), 0x77);
    assert_eq!(b.read8(0x0C00_0010, N), 0x77);
    // WAITCNT: SRAM 2 waits, WS0 N 2 / S 1, WS1 N 3 / S 1, WS2 N 8 / S 1.
    b.write16(
        WAITCNT,
        0b10 | 0b10 << 2 | 1 << 4 | 0b01 << 5 | 1 << 7 | 0b11 << 8 | 1 << 10,
        N,
    );
    assert_eq!(cycles(&mut b, |b| b.read16(0x0800_0000, N)), 3);
    assert_eq!(cycles(&mut b, |b| b.read16(0x0800_0002, S)), 2);
    assert_eq!(cycles(&mut b, |b| b.read32(0x0800_0000, N)), 5);
    assert_eq!(cycles(&mut b, |b| b.read16(0x0A00_0000, N)), 4);
    assert_eq!(cycles(&mut b, |b| b.read16(0x0A00_0002, S)), 2);
    assert_eq!(cycles(&mut b, |b| b.read16(0x0C00_0000, N)), 9);
    assert_eq!(cycles(&mut b, |b| b.read16(0x0C00_0002, S)), 2);
    assert_eq!(cycles(&mut b, |b| b.read8(0x0E00_0000, N)), 3);
    // Bit 15 is read-only and bits 11-12 (PHI) / 14 (prefetch) just store.
    b.write16(WAITCNT, 0xFFFF, N);
    assert_eq!(b.read16(WAITCNT, N), 0x7FFF);
}

#[test]
fn sequential_rom_access_restarts_at_a_128k_page() {
    let mut b = bus();
    assert_eq!(cycles(&mut b, |b| b.read16(0x0801_FFFE, S)), 3);
    assert_eq!(
        cycles(&mut b, |b| b.read16(0x0802_0000, S)),
        5,
        "page crossing is N"
    );
    assert_eq!(cycles(&mut b, |b| b.read16(0x0802_0002, S)), 3);
    // A word that starts a page is N+S even when the CPU calls it sequential.
    assert_eq!(cycles(&mut b, |b| b.read32(0x0804_0000, S)), 8);
    assert_eq!(cycles(&mut b, |b| b.read32(0x0804_0004, S)), 6);
}

#[test]
fn rom_beyond_the_image_reads_the_address() {
    let mut b = bus();
    assert_eq!(b.read16(0x0800_0200, N), 0x0100);
    assert_eq!(b.read16(0x0800_1000, N), 0x0800);
    assert_eq!(b.read32(0x0800_0204, N), 0x0103_0102);
    assert_eq!(b.read8(0x0800_0201, N), 0x01);
    assert_eq!(b.read16(0x09FF_FFFE, N), 0xFFFF);
}

#[test]
fn unmapped_regions_read_the_last_fetched_opcode() {
    let mut b = bus();
    b.write32(0x0300_0000, 0xE1A0_0000, N);
    assert_eq!(b.fetch32(0x0300_0000, N), 0xE1A0_0000);
    assert_eq!(b.read32(0x1000_0000, N), 0xE1A0_0000);
    assert_eq!(b.read16(0x1000_0002, N), 0xE1A0);
    assert_eq!(b.read8(0x0100_0001, N), 0x00);
    assert_eq!(b.read8(0x0100_0003, N), 0xE1);
    assert_eq!(cycles(&mut b, |b| b.read32(0x7000_0000, N)), 1);
    // A Thumb fetch replicates the halfword.
    b.write16(0x0300_0010, 0x4770, N);
    assert_eq!(b.fetch16(0x0300_0010, N), 0x4770);
    assert_eq!(b.read32(0x1000_0000, N), 0x4770_4770);
    // Unused I/O registers and the I/O gap read as open bus too.
    assert_eq!(b.read16(0x0400_0800, N), 0x4770);
    assert_eq!(b.read16(0x0400_00E0, N), 0x4770);
    // Writes to unmapped space are ignored.
    b.write32(0x1000_0000, 1, N);
}

#[test]
fn bios_reads_are_protected_outside_the_bios() {
    let mut bios = vec![0u8; 0x4000];
    bios[0..4].copy_from_slice(&0xE3A0_0001u32.to_le_bytes());
    bios[0xDC..0xE0].copy_from_slice(&0xAABB_CCDDu32.to_le_bytes());
    bios[0x100..0x104].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    let mut b = gba_core::bus::Bus::new(
        gba_core::cart::Cart::from_rom(rom_with(&[])).unwrap(),
        gba_core::bios::Bios::Real(bios.into_boxed_slice()),
    );
    // Running from ROM: BIOS reads return the post-startup latch.
    b.fetch32(0x0800_0000, N);
    assert_eq!(b.read32(0x0000_0100, N), 0xE129_F000);
    assert_eq!(b.read32(0x0000_0000, N), 0xE129_F000);
    // Fetching from the BIOS unlocks it and latches the fetched word.
    assert_eq!(b.fetch32(0x0000_00DC, N), 0xAABB_CCDD);
    assert_eq!(b.read32(0x0000_0100, N), 0x1234_5678);
    assert_eq!(b.read8(0x0000_0000, N), 0x01);
    // Back in ROM: protected again, returning the last BIOS fetch.
    b.fetch32(0x0800_0000, N);
    assert_eq!(b.read32(0x0000_0100, N), 0xAABB_CCDD);
    assert_eq!(b.read16(0x0000_0102, N), 0xAABB);
    // Past the end of the BIOS it's open bus (the ROM word just fetched).
    assert_eq!(b.read32(0x0000_4000, N), b.read32(0x0100_0000, N));
    // The test hook sees through the protection.
    assert_eq!(b.peek32(0x100), 0x1234_5678);
}

#[test]
fn interrupt_registers() {
    let mut b = bus();
    b.write16(0x0400_0200, 0xFFFF, N);
    assert_eq!(b.read16(0x0400_0200, N), 0x3FFF);
    b.irq.raise(Interrupt::VBlank);
    b.irq.raise(Interrupt::Timer1);
    assert_eq!(b.read16(0x0400_0202, N), 0b1_0001);
    assert!(!b.irq_line(), "IME is off");
    b.write16(0x0400_0208, 1, N);
    assert_eq!(b.read16(0x0400_0208, N), 1);
    assert!(b.irq_line());
    // Writing 1 acknowledges, 0 leaves it; byte lanes work.
    b.write16(0x0400_0202, 0b0_0001, N);
    assert_eq!(b.read16(0x0400_0202, N), 0b1_0000);
    b.write8(0x0400_0203, 0xFF, N);
    assert_eq!(b.read16(0x0400_0202, N), 0b1_0000);
    b.write8(0x0400_0202, 0x10, N);
    assert_eq!(b.read16(0x0400_0202, N), 0);
    assert!(!b.irq_line());
    // A 32-bit write to IE/IF sets IE and acknowledges in one go.
    b.irq.raise(Interrupt::HBlank);
    b.write32(0x0400_0200, 0x0002_0002, N);
    assert_eq!(b.read32(0x0400_0200, N), 0x0000_0002);
    // Unused halves read zero.
    assert_eq!(b.read16(0x0400_0206, N), 0);
    assert_eq!(b.read16(0x0400_020A, N), 0);
}

#[test]
fn halt_and_stop() {
    let mut b = bus();
    // HALTCNT is the byte at 0x301; POSTFLG the byte at 0x300 doesn't halt.
    b.write8(0x0400_0300, 1, N);
    assert!(!b.halted);
    assert_eq!(b.read8(0x0400_0300, N), 1);
    b.write8(0x0400_0301, 0, N);
    assert!(b.halted && !b.stopped);
    // Halt wakes on any enabled IF, regardless of IME.
    b.write16(0x0400_0200, 1 << 3, N);
    b.irq.raise(Interrupt::VBlank);
    b.process_events();
    assert!(b.halted, "VBlank isn't enabled");
    b.irq.raise(Interrupt::Timer0);
    b.process_events();
    assert!(!b.halted);
    // Stop only wakes on keypad / cartridge / serial.
    b.write16(0x0400_0200, 1 << 3 | 1 << 12, N);
    b.write8(0x0400_0301, 0x80, N);
    assert!(b.halted && b.stopped);
    b.irq.raise(Interrupt::Timer0);
    b.process_events();
    assert!(b.halted);
    b.irq.raise(Interrupt::Keypad);
    b.process_events();
    assert!(!b.halted && !b.stopped);
}

#[test]
fn halt_with_an_interrupt_already_pending_wakes_at_once() {
    let mut b = bus();
    b.write16(0x0400_0200, 1, N);
    b.irq.raise(Interrupt::VBlank);
    b.write8(0x0400_0301, 0, N);
    b.process_events();
    assert!(!b.halted);
}

#[test]
fn keypad_registers_and_interrupt() {
    let mut b = bus();
    assert_eq!(b.read16(0x0400_0130, N), 0x3FF);
    b.set_button(Button::A, true);
    assert_eq!(b.read16(0x0400_0130, N), 0x3FE);
    // OR mode, A or B.
    b.write16(0x0400_0132, 0x4000 | 0b11, N);
    assert_eq!(b.read16(0x0400_0132, N), 0x4003);
    assert_eq!(b.irq.if_ & 1 << 12, 1 << 12, "A already held when armed");
    b.irq.write_if(1 << 12);
    b.set_button(Button::B, true);
    assert_eq!(b.irq.if_ & 1 << 12, 0, "condition was already true");
    b.set_button(Button::A, false);
    b.set_button(Button::B, false);
    b.set_button(Button::B, true);
    assert_eq!(b.irq.if_ & 1 << 12, 1 << 12);
    // KEYINPUT is read-only.
    b.write16(0x0400_0130, 0, N);
    assert_eq!(b.read16(0x0400_0130, N), 0x3FD);
    // A stopped CPU wakes on it.
    b.irq.write_if(0xFFFF);
    b.write16(0x0400_0200, 1 << 12, N);
    b.set_button(Button::B, false);
    b.write8(0x0400_0301, 0x80, N);
    b.set_button(Button::B, true);
    assert!(!b.halted);
}

#[test]
fn write_only_and_sound_register_read_masks() {
    let mut b = bus();
    b.write32(0x0400_00D4, 0x0300_0000, N);
    assert_eq!(b.read32(0x0400_00D4, N), 0, "DMA SAD is write-only");
    assert_eq!(b.read16(0x0400_00D8, N), 0);
    b.write16(0x0400_0080, 0xFFFF, N);
    assert_eq!(b.read16(0x0400_0080, N), 0xFF77);
    b.write16(0x0400_0064, 0xFFFF, N);
    assert_eq!(b.read16(0x0400_0064, N), 0x4000);
    b.write16(0x0400_0088, 0xFFFF, N);
    assert_eq!(b.read16(0x0400_0088, N), 0xC3FE);
    b.write16(0x0400_0090, 0x1234, N);
    assert_eq!(b.read16(0x0400_0090, N), 0x1234, "wave RAM");
    b.write32(0x0400_00A0, 0x1234_5678, N);
    assert_eq!(b.read32(0x0400_00A0, N), 0, "FIFO is write-only");
}

#[test]
fn an_8bit_io_access_uses_one_lane() {
    let mut b = bus();
    b.write16(0x0400_0200, 0x1234, N);
    assert_eq!(b.read8(0x0400_0200, N), 0x34);
    assert_eq!(b.read8(0x0400_0201, N), 0x12);
    b.write8(0x0400_0201, 0x01, N);
    assert_eq!(b.read16(0x0400_0200, N), 0x0134);
    assert_eq!(
        cycles(&mut b, |b| b.read32(0x0400_0200, N)),
        1,
        "I/O is a 32-bit bus"
    );
}

#[test]
fn halt_with_a_pending_enabled_interrupt_does_not_halt() {
    let mut b = bus();
    b.write16(0x0400_0200, 1, N);
    b.irq.raise(Interrupt::VBlank);
    b.write8(0x0400_0301, 0, N);
    assert!(!b.halted, "no wait for the next event");
    // Stop likewise ends at once if a stop-wake interrupt is pending.
    b.write16(0x0400_0200, 1 << 12, N);
    b.irq.raise(Interrupt::Keypad);
    b.write8(0x0400_0301, 0x80, N);
    assert!(!b.halted && !b.stopped);
    // With nothing pending it halts.
    b.irq.write_if(0xFFFF);
    b.write8(0x0400_0301, 0, N);
    assert!(b.halted);
}

#[test]
fn only_halfword_data_reads_advance_the_eeprom() {
    let mut b = bus_with(rom_with(b"EEPROM_V124"));
    // A read request for a 512 B chip (9 bits), written by hand so the size is known.
    b.cart.eeprom_begin_write_dma(9);
    for bit in [1u16, 1, 0, 0, 0, 0, 0, 0, 0] {
        b.write16(0x0D00_0000, bit, N);
    }
    // Opcode fetches and byte/word reads must not shift bits out.
    b.fetch16(0x0D00_0000, N);
    b.fetch32(0x0D00_0000, N);
    b.read8(0x0D00_0000, N);
    b.read32(0x0D00_0000, N);
    // The 68-bit read still starts with its four leading zeros.
    for i in 0..4 {
        assert_eq!(b.read16(0x0D00_0000, N), 0, "bit {i}");
    }
    assert_eq!(
        b.read16(0x0D00_0000, N) & 1,
        1,
        "erased block: first data bit"
    );
}
