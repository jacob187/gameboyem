//! CGB register, banking and post-boot tests. ROMs here set the CGB flag
//! (0x143) explicitly: a flag-less ROM on a CGB runs in DMG-compatibility mode,
//! where most of these registers are unmapped (DECISIONS #33).

use super::*;
use crate::bus::{Bus, CpuBus};
use crate::cartridge;
use crate::cpu::Registers;

fn rom_with_flag(flag: u8) -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    rom[0x143] = flag;
    rom
}

/// A bus with the LCD off (so VRAM / OAM are never PPU-blocked).
fn bus(model: Model, flag: u8) -> Bus {
    let mut b = Bus::with_model(cartridge::from_rom(rom_with_flag(flag)).unwrap(), model);
    b.write(0xFF40, 0);
    b
}

fn cgb() -> Bus {
    bus(Model::Cgb, 0x80)
}

fn compat() -> Bus {
    bus(Model::Cgb, 0x00)
}

/// CGB-only registers a DMG must leave unmapped (reads 0xFF, writes dropped).
const CGB_ONLY: [u16; 14] = [
    0xFF4D, 0xFF4F, 0xFF70, 0xFF72, 0xFF73, 0xFF74, 0xFF75, 0xFF76, 0xFF77, 0xFF68, 0xFF69, 0xFF6A,
    0xFF6B, 0xFF51,
];

#[test]
fn model_from_header() {
    let h = |flag| cartridge::CartridgeHeader::parse(&rom_with_flag(flag)).unwrap();
    assert_eq!(Model::from_header(&h(0x80)), Model::Cgb);
    assert_eq!(Model::from_header(&h(0xC0)), Model::Cgb);
    assert_eq!(Model::from_header(&h(0x00)), Model::Dmg);
    assert_eq!(Model::from_header(&h(0x42)), Model::Dmg);
}

#[test]
fn the_header_title_and_the_model_agree_on_which_flags_are_cgb() {
    for flag in 0..=0xFFu8 {
        let mut rom = rom_with_flag(flag);
        rom[0x134..0x13F].fill(b'x');
        rom[0x13F..0x143].copy_from_slice(b"ABCD");
        let h = cartridge::CartridgeHeader::parse(&rom).unwrap();
        let cgb = Model::from_header(&h) == Model::Cgb;
        assert_eq!(cgb, matches!(flag, 0x80 | 0xC0), "{flag:02X}");
        // A CGB cart's title stops before the manufacturer code (0x13F..).
        assert_eq!(h.title.contains('A'), !cgb, "{flag:02X}");
    }
}

#[test]
fn model_ids_round_trip_and_unknown_ids_are_refused() {
    for model in [Model::Dmg, Model::Cgb] {
        assert_eq!(Model::from_id(model.id()), Some(model));
    }
    assert!((2..=u8::MAX).all(|id| Model::from_id(id).is_none()));
}

#[test]
fn dmg_leaves_every_cgb_register_unmapped() {
    // Even a CGB-flagged ROM: the model is explicit, never inferred.
    for flag in [0x00, 0x80, 0xC0] {
        let mut b = bus(Model::Dmg, flag);
        for addr in CGB_ONLY {
            assert_eq!(b.read(addr), 0xFF, "{addr:04X} flag {flag:02X}");
            b.write(addr, 0x01);
            assert_eq!(b.read(addr), 0xFF, "{addr:04X} after write");
        }
        // No banking: VBK / SVBK writes changed nothing.
        b.write(0x8000, 0x11);
        b.write(0xD000, 0x22);
        assert_eq!(b.ppu.vram_bank(0)[0], 0x11);
        assert_eq!(b.read(0xD000), 0x22);
        assert_eq!(b.wram[0x1000], 0x22, "D000 is the second 4 KiB of WRAM");
    }
}

#[test]
fn vbk_selects_the_vram_bank_for_cpu_peek_and_dma() {
    let mut b = cgb();
    b.write(0x8000, 0xA0);
    b.write(0x8123, 0xA1);
    b.write(0xFF4F, 1);
    b.write(0x8000, 0xB0);
    b.write(0x8123, 0xB1);
    assert_eq!(b.read(0x8000), 0xB0);
    assert_eq!(b.peek(0x8123), 0xB1);
    assert_eq!(b.ppu.vram_bank(0)[0], 0xA0);
    assert_eq!(b.ppu.vram_bank(0)[0x123], 0xA1);
    assert_eq!(b.ppu.vram_bank(1)[0], 0xB0);
    assert_eq!(b.ppu.vram_bank(1)[0x123], 0xB1);
    // OAM DMA from VRAM sees the selected bank.
    b.write(0xFF46, 0x80);
    for _ in 0..170 {
        b.tick();
    }
    assert_eq!(b.read(0xFE00), 0xB0);
    b.write(0xFF4F, 0);
    assert_eq!(b.read(0x8000), 0xA0);
    b.write(0xFF46, 0x81);
    for _ in 0..170 {
        b.tick();
    }
    assert_eq!(b.read(0xFE23), 0xA1);
}

#[test]
fn vbk_reads_back_one_bits_and_only_bit_0_is_stored() {
    let mut b = cgb();
    assert_eq!(b.read(0xFF4F), 0xFE);
    b.write(0xFF4F, 0xFF);
    assert_eq!(b.read(0xFF4F), 0xFF);
    b.write(0xFF4F, 0xFE);
    assert_eq!(b.read(0xFF4F), 0xFE);
    b.write(0xFF4F, 0x03);
    assert_eq!(b.read(0xFF4F), 0xFF);
}

#[test]
fn wram_banks_svbk_and_echo() {
    let mut b = cgb();
    assert_eq!(b.read(0xFF70), 0xF8, "post-boot SVBK");
    b.write(0xC000, 0x10);
    b.write(0xCFFF, 0x11);
    for bank in 1..8u8 {
        b.write(0xFF70, bank);
        assert_eq!(b.read(0xFF70), 0xF8 | bank);
        b.write(0xD000, 0x20 + bank);
        b.write(0xDFFF, 0x30 + bank);
    }
    for bank in 1..8u8 {
        b.write(0xFF70, bank);
        assert_eq!(b.read(0xD000), 0x20 + bank, "bank {bank}");
        assert_eq!(b.read(0xDFFF), 0x30 + bank, "bank {bank}");
        // C000-CFFF is bank 0 whatever is selected.
        assert_eq!(b.read(0xC000), 0x10);
        assert_eq!(b.read(0xCFFF), 0x11);
        // Echo: E000-EFFF mirrors C000-CFFF, F000-FDFF the selected D000 window.
        assert_eq!(b.read(0xE000), 0x10);
        assert_eq!(b.read(0xEFFF), 0x11);
        assert_eq!(b.read(0xF000), 0x20 + bank);
    }
    // Bank 0 selects bank 1; only bits 0-2 count.
    b.write(0xFF70, 1);
    let one = b.read(0xD000);
    b.write(0xFF70, 0);
    assert_eq!(b.read(0xFF70), 0xF8);
    assert_eq!(b.read(0xD000), one);
    b.write(0xFF70, 0xF9);
    assert_eq!(b.read(0xFF70), 0xF9);
    assert_eq!(b.read(0xD000), one);
    // Writes through the echo land in the real bank.
    b.write(0xFF70, 3);
    b.write(0xF010, 0x77);
    assert_eq!(b.read(0xD010), 0x77);
    b.write(0xE020, 0x66);
    assert_eq!(b.read(0xC020), 0x66);
}

#[test]
fn dma_source_uses_the_selected_wram_bank_and_echo() {
    let mut b = cgb();
    b.write(0xFF70, 5);
    b.write(0xD000, 0x5D);
    b.write(0xC000, 0x0C);
    b.write(0xFF70, 2);
    b.write(0xD000, 0x2D);
    b.write(0xFF70, 5);
    b.write(0xFF46, 0xD0);
    for _ in 0..170 {
        b.tick();
    }
    assert_eq!(b.read(0xFE00), 0x5D);
    // A source above DFFF maps onto the echo of C000-DFFF.
    b.write(0xFF46, 0xE0);
    for _ in 0..170 {
        b.tick();
    }
    assert_eq!(b.read(0xFE00), 0x0C);
    b.write(0xFF46, 0xF0);
    for _ in 0..170 {
        b.tick();
    }
    assert_eq!(b.read(0xFE00), 0x5D);
}

#[test]
fn key1_is_a_prepare_bit_with_unused_bits_set() {
    let mut b = cgb();
    assert_eq!(b.read(0xFF4D), 0x7E);
    b.write(0xFF4D, 0xFF);
    assert_eq!(
        b.read(0xFF4D),
        0x7F,
        "bit 7 is the current speed, not writable"
    );
    b.write(0xFF4D, 0xFE);
    assert_eq!(b.read(0xFF4D), 0x7E);
}

#[test]
fn ff74_is_a_plain_register_in_cgb_mode_only() {
    let mut b = cgb();
    assert_eq!(b.read(0xFF74), 0x00);
    for v in [0xA5, 0xFF, 0x00, 0x81] {
        b.write(0xFF74, v);
        assert_eq!(b.read(0xFF74), v);
        assert_eq!(b.peek(0xFF74), v);
    }
    // It is independent of its neighbours.
    b.write(0xFF74, 0x3C);
    b.write(0xFF73, 0x11);
    b.write(0xFF75, 0x70);
    assert_eq!(b.read(0xFF74), 0x3C);
    for model_hw in [bus(Model::Dmg, 0x80), compat()] {
        let mut b = model_hw;
        b.write(0xFF74, 0x3C);
        assert_eq!(b.read(0xFF74), 0xFF);
    }
}

#[test]
fn compat_mode_maps_only_vbk_and_the_undocumented_registers() {
    let mut b = compat();
    // Unmapped like on a DMG: KEY1, SVBK, HDMA, RP, palette data, OPRI, FF74.
    for addr in [
        0xFF4C, 0xFF4D, 0xFF4E, 0xFF51, 0xFF55, 0xFF56, 0xFF69, 0xFF6B, 0xFF6C, 0xFF70, 0xFF74,
    ] {
        b.write(addr, 0x01);
        assert_eq!(b.read(addr), 0xFF, "{addr:04X}");
    }
    // WRAM is not banked; VBK is locked to bank 0.
    b.write(0xD000, 0x44);
    assert_eq!(b.wram[0x1000], 0x44);
    b.write(0xFF4F, 1);
    assert_eq!(b.read(0xFF4F), 0xFE);
    b.write(0x8000, 0x55);
    assert_eq!(b.ppu.vram_bank(0)[0], 0x55);
    // FF72/FF73 are plain, FF75 keeps bits 4-6, FF76/77 read the (silent) PCM.
    b.write(0xFF72, 0xA5);
    b.write(0xFF73, 0x5A);
    assert_eq!((b.read(0xFF72), b.read(0xFF73)), (0xA5, 0x5A));
    b.write(0xFF75, 0x00);
    assert_eq!(b.read(0xFF75), 0x8F);
    b.write(0xFF75, 0xFF);
    assert_eq!(b.read(0xFF75), 0xFF);
    assert_eq!((b.read(0xFF76), b.read(0xFF77)), (0, 0));
}

#[test]
fn pcm_registers_follow_the_apu() {
    let mut b = compat();
    // CH2: duty 50%, volume 15, period such that it is in the high phase at some point.
    b.write(0xFF26, 0x80);
    b.write(0xFF17, 0xF0);
    b.write(0xFF16, 0x80);
    b.write(0xFF18, 0x00);
    b.write(0xFF19, 0x80 | 0x07);
    let mut seen = 0;
    for _ in 0..200 {
        b.tick();
        seen |= b.read(0xFF76);
    }
    assert_eq!(seen & 0x0F, 0, "CH1 is silent");
    assert_eq!(seen >> 4, 0x0F, "CH2 reaches volume 15");
    assert_eq!(b.read(0xFF77), 0);
}

#[test]
fn post_boot_registers_per_model_and_mode() {
    let regs = |model, flag| {
        Bus::with_model(cartridge::from_rom(rom_with_flag(flag)).unwrap(), model)
            .post_boot_registers()
    };
    // Explicit model: a CGB-flagged ROM on a Dmg is still a DMG.
    assert_eq!(regs(Model::Dmg, 0x80), Registers::dmg_post_boot());
    assert_eq!(regs(Model::Dmg, 0x00), Registers::dmg_post_boot());
    let r = regs(Model::Cgb, 0x80);
    assert_eq!(
        (r.a, r.f, r.b, r.c, r.d, r.e, r.h, r.l, r.sp, r.pc),
        (0x11, 0x80, 0, 0, 0xFF, 0x56, 0x00, 0x0D, 0xFFFE, 0x0100)
    );
    assert_eq!(regs(Model::Cgb, 0xC0), r);
    let r = regs(Model::Cgb, 0x00);
    assert_eq!(
        (r.a, r.f, r.b, r.c, r.d, r.e, r.h, r.l),
        (0x11, 0x80, 0, 0, 0, 0x08, 0x00, 0x7C)
    );
}

#[test]
fn compat_b_is_the_title_sum_for_licensee_01() {
    let regs = |patch: &dyn Fn(&mut Vec<u8>)| {
        let mut rom = rom_with_flag(0);
        patch(&mut rom);
        Bus::with_model(cartridge::from_rom(rom).unwrap(), Model::Cgb).post_boot_registers()
    };
    // Old licensee 0x01: B = sum of 0x134..=0x143; 0x43 or 0x58 selects HL = 991A.
    let r = regs(&|rom| {
        rom[0x14B] = 0x01;
        rom[0x134] = 0x40;
        rom[0x135] = 0x03;
    });
    assert_eq!((r.b, r.h, r.l), (0x43, 0x99, 0x1A));
    let r = regs(&|rom| {
        rom[0x14B] = 0x01;
        rom[0x134] = 0x10;
    });
    assert_eq!((r.b, r.h, r.l), (0x10, 0x00, 0x7C));
    // 0x33 plus the new licensee code "01" counts too; anything else leaves B = 0.
    let r = regs(&|rom| {
        rom[0x14B] = 0x33;
        rom[0x144] = b'0';
        rom[0x145] = b'1';
        rom[0x134] = 0x58;
    });
    assert_eq!((r.b, r.h, r.l), (0x58, 0x99, 0x1A));
    let r = regs(&|rom| {
        rom[0x14B] = 0x33;
        rom[0x134] = 0x58;
    });
    assert_eq!(r.b, 0);
}

/// mooneye `misc/boot_hwio-C` data (rows of a mask byte, then 8 values):
/// registers a CGB in DMG-compatibility mode shows at hand-off. Skipped
/// (mask 0) are DIV, wave RAM, LCDC, STAT, LY, DMA and OBP0/1.
const BOOT_HWIO_C: [[u8; 9]; 16] = [
    [0b1111_0111, 0xFF, 0x00, 0x7E, 0xFF, 0xFF, 0x00, 0x00, 0xF8],
    [0b1111_1111, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xE1],
    [0b1111_1111, 0x80, 0xBF, 0xF3, 0xFF, 0xBF, 0xFF, 0x3F, 0x00],
    [0b1111_1111, 0xFF, 0xBF, 0x7F, 0xFF, 0x9F, 0xFF, 0xBF, 0xFF],
    [0b1111_1111, 0xFF, 0x00, 0x00, 0xBF, 0x77, 0xF3, 0xF1, 0xFF],
    [0b1111_1111, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
    [0b0000_0000, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
    [0b0000_0000, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
    [0b0011_0101, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFC],
    [0b0011_1111, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFE],
    [0b1111_1111, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
    [0b1111_1111, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
    [0b1111_1111, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
    [0b1111_1111, 0xC8, 0xFF, 0xD0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
    [0b1111_1111, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0x8F, 0x00, 0x00],
    [0b1111_1111, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
];

#[test]
fn compat_mode_post_boot_io_matches_mooneye_boot_hwio_c() {
    let gb = crate::GameBoy::with_model(rom_with_flag(0), Model::Cgb).unwrap();
    for (row, data) in BOOT_HWIO_C.iter().enumerate() {
        for i in 0..8 {
            let addr = 0xFF00 + (row * 8 + i) as u16;
            if data[0] & (0x80 >> i) != 0 {
                assert_eq!(gb.peek(addr), data[1 + i], "{addr:04X}");
            }
        }
    }
}

#[test]
fn cgb_mode_post_boot_io() {
    let gb = crate::GameBoy::with_model(rom_with_flag(0x80), Model::Cgb).unwrap();
    // Pan Docs "Power Up Sequence", CGB column.
    for (addr, v) in [
        (0xFF00, 0xCF),
        (0xFF01, 0x00),
        (0xFF02, 0x7F),
        (0xFF05, 0x00),
        (0xFF07, 0xF8),
        (0xFF0F, 0xE1),
        (0xFF26, 0xF1),
        (0xFF40, 0x91),
        (0xFF45, 0x00),
        (0xFF46, 0x00),
        (0xFF47, 0xFC),
        (0xFF4D, 0x7E),
        (0xFF4F, 0xFE),
        (0xFF70, 0xF8),
        (0xFFFF, 0x00),
    ] {
        assert_eq!(gb.peek(addr), v, "{addr:04X}");
    }
    assert_eq!(gb.peek(0xFF44), 0x90, "LY");
}

#[test]
fn dmg_post_boot_state_is_untouched() {
    let gb = crate::GameBoy::new(rom_with_flag(0x80)).unwrap();
    assert_eq!(gb.model(), Model::Dmg);
    assert_eq!(gb.cpu_registers(), Registers::dmg_post_boot());
    assert_eq!(gb.peek(0xFF44), 0, "LY=153 quirk window reads 0");
    assert_eq!(gb.peek(0xFF41), 0x85);
}

#[test]
fn cgb_mode_maps_the_palette_ports_and_opri() {
    let mut b = cgb();
    assert_eq!((b.read(0xFF68), b.read(0xFF6A)), (0xC8, 0xD0));
    assert_eq!(b.read(0xFF6C), 0xFE);
    // The LCD is in VBlank at hand-off, so the ports are open.
    b.write(0xFF68, 0x80 | 2);
    b.write(0xFF69, 0x34);
    b.write(0xFF69, 0x12);
    assert_eq!(b.read(0xFF68), 0xC4);
    b.write(0xFF68, 2);
    assert_eq!((b.read(0xFF69), b.read(0xFF68)), (0x34, 0x42));
    b.write(0xFF6A, 0x80);
    b.write(0xFF6B, 0xAA);
    b.write(0xFF6A, 0);
    assert_eq!(b.read(0xFF6B), 0xAA);
    b.write(0xFF6C, 0xFF);
    assert_eq!(b.read(0xFF6C), 0xFF);
    // A colour-mode machine has RGB555 frames; the others do not.
    assert!(b.ppu.cgb_framebuffer().is_some());
    assert!(compat().ppu.cgb_framebuffer().is_none());
    assert!(bus(Model::Dmg, 0x80).ppu.cgb_framebuffer().is_none());
}

#[test]
fn compat_mode_has_plain_palette_index_registers_and_no_data_ports() {
    let mut b = compat();
    assert_eq!((b.read(0xFF68), b.read(0xFF6A)), (0xC8, 0xD0));
    b.write(0xFF68, 0x00);
    b.write(0xFF6A, 0x05);
    assert_eq!((b.read(0xFF68), b.read(0xFF6A)), (0x40, 0x45));
    // The data ports read 0xFF and drop writes, with no auto-increment.
    b.write(0xFF68, 0x80);
    b.write(0xFF69, 0x12);
    assert_eq!((b.read(0xFF69), b.read(0xFF68)), (0xFF, 0xC0));
    assert_eq!(b.read(0xFF6C), 0xFF);
}
