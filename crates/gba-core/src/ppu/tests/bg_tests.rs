//! Bitmap and text background tests.

use super::*;

const BG2_ON: u16 = 0x0400;
const FRAME1: u16 = 0x0010;

#[test]
fn forced_blank_is_white_and_a_disabled_screen_shows_the_backdrop() {
    let mut rig = Rig::new();
    rig.pal(0, rgb(1, 2, 3));
    rig.write(DISPCNT, 0x0080 | 3 | BG2_ON);
    let f = rig.frame();
    assert!(f.iter().all(|&p| p == 0x7FFF));
    rig.write(DISPCNT, 3);
    let f = rig.frame();
    assert!(f.iter().all(|&p| p == rgb(1, 2, 3)));
}

#[test]
fn mode3_is_240x160_direct_colour() {
    let mut rig = Rig::new();
    rig.pal(0, rgb(31, 0, 31));
    rig.vram16(0, rgb(31, 0, 0));
    rig.vram16((159 * 240 + 239) * 2, rgb(0, 0, 31));
    rig.vram16((5 * 240 + 7) * 2, 0x8000 | rgb(0, 31, 0));
    rig.write(DISPCNT, 3 | BG2_ON);
    let f = rig.frame();
    assert_eq!(px(&f, 0, 0), rgb(31, 0, 0));
    assert_eq!(px(&f, 239, 159), rgb(0, 0, 31));
    // Bit 15 of a direct colour is ignored.
    assert_eq!(px(&f, 7, 5), rgb(0, 31, 0));
    // Untouched VRAM is colour 0 (black), opaque: the backdrop does not show.
    assert_eq!(px(&f, 100, 100), 0);
}

#[test]
fn mode4_uses_the_palette_and_page_flips() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    rig.pal(0, rgb(31, 31, 0));
    rig.ppu.vram[2 * 240 + 3] = 5;
    rig.ppu.vram[0xA000 + 2 * 240 + 3] = 9;
    rig.ppu.vram[239 + 159 * 240] = 200;
    rig.write(DISPCNT, 4 | BG2_ON);
    let f = rig.frame();
    assert_eq!(px(&f, 3, 2), pal_colour(5));
    assert_eq!(px(&f, 239, 159), pal_colour(200));
    // Index 0 is transparent: the backdrop (palette entry 0) shows.
    assert_eq!(px(&f, 0, 0), rgb(31, 31, 0));
    rig.write(DISPCNT, 4 | BG2_ON | FRAME1);
    let f = rig.frame();
    assert_eq!(px(&f, 3, 2), pal_colour(9));
    assert_eq!(px(&f, 239, 159), rgb(31, 31, 0));
}

#[test]
fn mode5_is_160x128_with_backdrop_outside_and_two_frames() {
    let mut rig = Rig::new();
    rig.pal(0, rgb(0, 31, 31));
    rig.vram16(0, rgb(31, 0, 0));
    rig.vram16((127 * 160 + 159) * 2, rgb(0, 0, 31));
    rig.vram16(0xA000, rgb(0, 31, 0));
    rig.write(DISPCNT, 5 | BG2_ON);
    let f = rig.frame();
    assert_eq!(px(&f, 0, 0), rgb(31, 0, 0));
    assert_eq!(px(&f, 159, 127), rgb(0, 0, 31));
    assert_eq!(px(&f, 160, 0), rgb(0, 31, 31), "right of the bitmap");
    assert_eq!(px(&f, 0, 128), rgb(0, 31, 31), "below the bitmap");
    assert_eq!(px(&f, 239, 159), rgb(0, 31, 31));
    rig.write(DISPCNT, 5 | BG2_ON | FRAME1);
    let f = rig.frame();
    assert_eq!(px(&f, 0, 0), rgb(0, 31, 0));
    assert_eq!(px(&f, 159, 127), 0);
}

#[test]
fn bitmap_modes_need_the_bg2_enable_bit() {
    for mode in 3..=5 {
        let mut rig = Rig::new();
        rig.pal(0, rgb(4, 5, 6));
        rig.ppu.vram.fill(0xFF);
        rig.write(DISPCNT, mode);
        let f = rig.frame();
        assert!(f.iter().all(|&p| p == rgb(4, 5, 6)), "mode {mode}");
    }
}

#[test]
fn text_4bpp_tile_palette_bank_and_flips() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    // BG0: char base 0, screen base 8 (0x4000), 4bpp.
    rig.write(0x008, 8 << 8);
    tile4(&mut rig, 0, 1);
    // Cells: (0,0) plain bank 0; (1,0) bank 3; (2,0) hflip; (3,0) vflip; (4,0) both.
    let entries = [1u16, 1 | 3 << 12, 1 | 0x400, 1 | 0x800, 1 | 0xC00];
    for (i, e) in entries.iter().enumerate() {
        rig.vram16(0x4000 + i * 2, *e);
    }
    rig.write(DISPCNT, bg_on(0));
    let f = rig.frame();
    for r in 0..8 {
        for c in 0..8 {
            assert_eq!(px(&f, c, r), pal_colour(t4(r, c)), "plain {c},{r}");
            assert_eq!(
                px(&f, 8 + c, r),
                pal_colour(3 * 16 + t4(r, c)),
                "bank {c},{r}"
            );
            assert_eq!(px(&f, 16 + c, r), pal_colour(t4(r, 7 - c)), "hflip {c},{r}");
            assert_eq!(px(&f, 24 + c, r), pal_colour(t4(7 - r, c)), "vflip {c},{r}");
            assert_eq!(
                px(&f, 32 + c, r),
                pal_colour(t4(7 - r, 7 - c)),
                "hvflip {c},{r}"
            );
        }
    }
    // Tile 0 is empty: index 0 is transparent, so the backdrop shows.
    assert_eq!(px(&f, 40, 0), pal_colour(0));
}

#[test]
fn text_8bpp_ignores_the_palette_bank() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    // BG1: 8bpp, char base 1 (0x4000), screen base 4 (0x2000).
    rig.write(0x00A, 1 << 2 | 1 << 7 | 4 << 8);
    for r in 0..8 {
        for c in 0..8 {
            // 8bpp tile 2 occupies 64 bytes at 0x4000 + 2 * 64.
            rig.ppu.vram[0x4000 + 2 * 64 + r * 8 + c] = (10 + r * 8 + c) as u8;
        }
    }
    rig.vram16(0x2000, 2 | 7 << 12);
    rig.vram16(0x2002, 2 | 0x400 | 0x800);
    rig.write(DISPCNT, bg_on(1));
    let f = rig.frame();
    for r in 0..8 {
        for c in 0..8 {
            assert_eq!(px(&f, c, r), pal_colour(10 + r * 8 + c));
            assert_eq!(px(&f, 8 + c, r), pal_colour(10 + (7 - r) * 8 + (7 - c)));
        }
    }
}

/// An 8bpp tile `n` filled with palette index `n`, for n in 1..=200, at the
/// character base `char_base`.
fn solid_tiles(rig: &mut Rig, char_base: usize) {
    for n in 1..=200usize {
        rig.ppu.vram[char_base + n * 64..char_base + n * 64 + 64].fill(n as u8);
    }
}

/// The tile number used at map cell (tx, ty) of the scroll tests.
fn cell_tile(tx: usize, ty: usize) -> usize {
    1 + (tx + ty * 5) % 200
}

/// VRAM offset of map cell (tx, ty) for screen size `size`, from GBATEK "BG Screen
/// Data Format": 32x32 blocks of 0x800 bytes; 64-wide maps put blocks side by side,
/// 64-high maps below, 64x64 is 2x2.
fn cell_addr(screen_base: usize, size: usize, tx: usize, ty: usize) -> usize {
    let (bx, by) = (tx / 32, ty / 32);
    let block = match size {
        0 => 0,
        1 => bx,
        2 => by,
        _ => bx + 2 * by,
    };
    screen_base + block * 0x800 + ((ty % 32) * 32 + tx % 32) * 2
}

#[test]
fn text_scroll_and_wrap_for_every_screen_size() {
    for size in 0..4usize {
        let (wt, ht) = (
            if size & 1 != 0 { 64 } else { 32 },
            if size & 2 != 0 { 64 } else { 32 },
        );
        let mut rig = Rig::new();
        rig.distinct_palette();
        solid_tiles(&mut rig, 0x4000);
        // BG0: 8bpp, char base 1, screen base 16 (0x8000), size `size`.
        rig.write(0x008, 1 << 2 | 1 << 7 | 16 << 8 | (size as u16) << 14);
        for ty in 0..ht {
            for tx in 0..wt {
                rig.vram16(cell_addr(0x8000, size, tx, ty), cell_tile(tx, ty) as u16);
            }
        }
        // Scroll so the screen's top-left is 4 pixels before the map's far corner.
        let (hofs, vofs) = (wt * 8 - 4, ht * 8 - 4);
        rig.write(0x010, hofs as u16);
        rig.write(0x012, vofs as u16);
        rig.write(DISPCNT, bg_on(0));
        let f = rig.frame();
        let at = |x: usize, y: usize| {
            let (mx, my) = ((x + hofs) % (wt * 8), (y + vofs) % (ht * 8));
            pal_colour(cell_tile(mx / 8, my / 8))
        };
        for (x, y) in [
            (0, 0),
            (3, 0),
            (4, 0),
            (0, 3),
            (0, 4),
            (4, 4),
            (20, 20),
            (239, 159),
            (100, 77),
        ] {
            assert_eq!(px(&f, x, y), at(x, y), "size {size} pixel {x},{y}");
        }
    }
}

#[test]
fn text_fine_scroll_within_a_tile() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    // 4bpp tile with a distinct colour per column: idx = 1 + c.
    for r in 0..8 {
        for c in 0..4 {
            rig.ppu.vram[32 + r * 4 + c] = ((1 + 2 * c) | (2 + 2 * c) << 4) as u8;
        }
    }
    for tx in 0..32 {
        rig.vram16(0x800 + tx * 2, 1);
    }
    rig.write(0x008, 1 << 8);
    rig.write(0x010, 3);
    rig.write(DISPCNT, bg_on(0));
    let f = rig.frame();
    // Screen x 0 shows map column 3 of the tile (idx 4); x 5 starts the next tile.
    assert_eq!(px(&f, 0, 0), pal_colour(4));
    assert_eq!(px(&f, 4, 0), pal_colour(8));
    assert_eq!(px(&f, 5, 0), pal_colour(1));
    // Rightmost pixel: map x = 239 + 3 = 242, column 2 of its tile.
    assert_eq!(px(&f, 239, 0), pal_colour(3));
}

#[test]
fn text_bg_bases_select_char_and_screen_blocks() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    tile4(&mut rig, 0xC000, 5);
    rig.write(0x00C, 3 << 2 | 31 << 8);
    rig.vram16(31 * 0x800, 5);
    rig.write(DISPCNT, bg_on(2));
    let f = rig.frame();
    assert_eq!(px(&f, 0, 0), pal_colour(t4(0, 0)));
    assert_eq!(px(&f, 7, 7), pal_colour(t4(7, 7)));
}

#[test]
fn bg_tile_data_beyond_64k_is_not_shown() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    // Char base 3, 4bpp tile 1000 is at 0xC000 + 32000 = 0x13D00: OBJ memory.
    rig.ppu.vram[0x13D00..0x13D20].fill(0x11);
    rig.write(0x008, 3 << 2 | 1 << 8);
    rig.vram16(0x800, 1000);
    rig.write(DISPCNT, bg_on(0));
    let f = rig.frame();
    assert_eq!(px(&f, 0, 0), pal_colour(0));
}

#[test]
fn bg_priority_then_index_orders_the_layers() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    for bg in 0..4 {
        solid_bg(&mut rig, bg, 20 + bg as u8, 3);
    }
    rig.write(DISPCNT, 0x0F00);
    // All priority 3: BG0 is in front.
    assert_eq!(px(&rig.frame(), 10, 10), pal_colour(20));
    // BG2 and BG3 at priority 1: BG2 (lower index) wins over BG3 and over BG0/1.
    solid_bg(&mut rig, 2, 22, 1);
    solid_bg(&mut rig, 3, 23, 1);
    assert_eq!(px(&rig.frame(), 10, 10), pal_colour(22));
    // BG3 at priority 0 beats everyone.
    solid_bg(&mut rig, 3, 23, 0);
    assert_eq!(px(&rig.frame(), 10, 10), pal_colour(23));
    // Disabling it reveals BG2 again.
    rig.write(DISPCNT, 0x0700);
    assert_eq!(px(&rig.frame(), 10, 10), pal_colour(22));
}

#[test]
fn mode1_has_no_bg3() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    solid_bg(&mut rig, 0, 30, 2);
    solid_bg(&mut rig, 1, 31, 1);
    solid_bg(&mut rig, 3, 33, 0);
    rig.write(DISPCNT, 1 | 0x0B00);
    // BG3 does not exist in mode 1; BG2 is affine (see affine_tests).
    assert_eq!(px(&rig.frame(), 5, 5), pal_colour(31));
}
