//! Mosaic tests: BG and OBJ mosaic through the MOSAIC register.

use super::*;

const MOSAIC: u32 = 0x04C;
const BG0CNT: u32 = 0x008;
const BG2CNT: u32 = 0x00C;
const BG2PA: u32 = 0x020;
const MOSAIC_BIT: u16 = 0x40;
const OBJ_ON: u16 = 0x1000;
const ONE_D: u16 = 0x40;

/// MOSAIC value for BG sizes (h, v) and OBJ sizes (oh, ov), all in pixels (1..=16).
fn mosaic(h: u16, v: u16, oh: u16, ov: u16) -> u16 {
    (h - 1) | (v - 1) << 4 | (oh - 1) << 8 | (ov - 1) << 12
}

/// A scrolled 4bpp text BG0 patterned with the shared test tile.
fn text_bg(rig: &mut Rig, mosaic_on: bool) {
    rig.distinct_palette();
    tile4(rig, 0, 1);
    for i in 0..32 * 32 {
        rig.vram16(0x4000 + i * 2, 1);
    }
    rig.write(BG0CNT, 8 << 8 | if mosaic_on { MOSAIC_BIT } else { 0 });
    rig.write(0x010, 3);
    rig.write(0x012, 2);
    rig.write(DISPCNT, bg_on(0));
}

#[test]
fn bg_mosaic_repeats_the_top_left_pixel_of_each_block() {
    let mut base = Rig::new();
    text_bg(&mut base, true);
    let plain = base.frame();
    for (h, v) in [(1, 1), (2, 2), (4, 3), (16, 16), (1, 8), (7, 1)] {
        let mut rig = Rig::new();
        text_bg(&mut rig, true);
        rig.write(MOSAIC, mosaic(h, v, 1, 1));
        let f = rig.frame();
        for y in 0..160usize {
            for x in 0..240usize {
                let (sx, sy) = (x - x % h as usize, y - y % v as usize);
                assert_eq!(px(&f, x, y), px(&plain, sx, sy), "{h}x{v} at ({x},{y})");
            }
        }
    }
}

#[test]
fn bg_mosaic_absolute_pixels() {
    let mut rig = Rig::new();
    text_bg(&mut rig, true);
    rig.write(MOSAIC, mosaic(4, 4, 1, 1));
    let f = rig.frame();
    // Scroll (3, 2): screen (x, y) shows tile pixel row (y + 2) % 8, column (x + 3) % 8.
    // The block at screen (4..8, 4..8) takes its top-left pixel (4, 4): row 6, col 7.
    for (x, y) in [(4, 4), (7, 7), (5, 6)] {
        assert_eq!(px(&f, x, y), pal_colour(t4(6, 7)), "({x},{y})");
    }
    // The next block over: screen (8, 4) -> row 6, col 3.
    assert_eq!(px(&f, 11, 4), pal_colour(t4(6, 3)));
}

#[test]
fn bg_mosaic_needs_the_per_bg_bit_and_ignores_the_obj_sizes() {
    let mut off = Rig::new();
    text_bg(&mut off, false);
    off.write(MOSAIC, mosaic(8, 8, 1, 1));
    let a = off.frame();
    let mut plain = Rig::new();
    text_bg(&mut plain, false);
    assert_eq!(a, plain.frame(), "bit 6 clear: no mosaic");
    // OBJ-only sizes leave a mosaic BG alone.
    let mut obj_only = Rig::new();
    text_bg(&mut obj_only, true);
    obj_only.write(MOSAIC, mosaic(1, 1, 8, 8));
    assert_eq!(obj_only.frame(), a);
}

#[test]
fn bitmap_bg_mosaic() {
    let mut rig = Rig::new();
    for y in 0..160usize {
        for x in 0..240usize {
            rig.vram16((y * 240 + x) * 2, (x * 7 + y * 13) as u16 & 0x7FFF);
        }
    }
    rig.write(BG2CNT, MOSAIC_BIT);
    rig.write(MOSAIC, mosaic(5, 3, 1, 1));
    rig.write(DISPCNT, 3 | bg_on(2));
    let f = rig.frame();
    for (x, y) in [(0, 0), (4, 2), (5, 3), (12, 8), (239, 159), (237, 158)] {
        let (sx, sy) = (x - x % 5, y - y % 3);
        assert_eq!(
            px(&f, x, y),
            (sx * 7 + sy * 13) as u16 & 0x7FFF,
            "({x},{y})"
        );
    }
}

#[test]
fn affine_bg_mosaic_holds_the_block_reference_point() {
    // A shearing, scaling matrix: with mosaic every pixel of a block shows the block's
    // top-left pixel, as for text BGs.
    let build = |mos: bool| {
        let mut rig = Rig::new();
        rig.distinct_palette();
        for t in 0..256usize {
            for i in 0..64 {
                rig.ppu.vram[t * 64 + i] = (1 + (t * 5 + i) % 250) as u8;
            }
        }
        for i in 0..16 * 16 {
            rig.ppu.vram[0x4000 + i] = (i * 7 % 256) as u8;
        }
        rig.write(BG2CNT, 8 << 8 | 0x2000 | if mos { MOSAIC_BIT } else { 0 });
        for (i, v) in [0x120i16, 0x30, -0x20, 0xE0].into_iter().enumerate() {
            rig.write(BG2PA + 2 * i as u32, v as u16);
        }
        rig.write(DISPCNT, 2 | bg_on(2));
        rig
    };
    let plain = build(false).frame();
    let mut rig = build(true);
    rig.write(MOSAIC, mosaic(3, 4, 1, 1));
    let f = rig.frame();
    for y in 0..160usize {
        for x in 0..240usize {
            assert_eq!(px(&f, x, y), px(&plain, x - x % 3, y - y % 4), "({x},{y})");
        }
    }
}

fn patterned_sprite_tiles(rig: &mut Rig) {
    for n in 0..1024usize {
        for r in 0..8 {
            for c in 0..4 {
                let lo = 1 + (r * 3 + 2 * c + n) % 15;
                let hi = 1 + (r * 3 + 2 * c + 1 + n) % 15;
                rig.ppu.vram[0x1_0000 + n * 32 + r * 4 + c] = (lo | hi << 4) as u8;
            }
        }
    }
}

/// Colour of texel (u, v) of a 16x16 1D 4bpp sprite with first tile `base`.
fn sprite_texel(base: usize, u: usize, v: usize) -> u16 {
    let n = base + (v / 8) * 2 + u / 8;
    pal_colour(256 + 1 + ((v % 8) * 3 + (u % 8) + n) % 15)
}

fn sprite_rig(x: u16, attr0_extra: u16, mosaic_reg: u16) -> Rig {
    let mut rig = Rig::new();
    rig.distinct_palette();
    rig.hide_all_sprites();
    patterned_sprite_tiles(&mut rig);
    rig.oam(0, 30 | attr0_extra, x | 1 << 14, 100);
    rig.write(MOSAIC, mosaic_reg);
    rig.write(DISPCNT, OBJ_ON | ONE_D);
    rig
}

#[test]
fn obj_mosaic_quantises_sprite_pixels_to_screen_aligned_blocks() {
    // attr0 bit 12 = OBJ mosaic; blocks 4 wide, 3 high; sprite at (40, 30).
    let mut rig = sprite_rig(40, 0x1000, mosaic(1, 1, 4, 3));
    let f = rig.frame();
    for y in 30..46usize {
        for x in 40..56usize {
            let (u, v) = (x - x % 4 - 40, (y - y % 3).saturating_sub(30));
            assert_eq!(px(&f, x, y), sprite_texel(100, u, v), "({x},{y})");
        }
    }
    assert_eq!(px(&f, 56, 30), pal_colour(0));
}

#[test]
fn obj_mosaic_block_starting_left_of_the_sprite_uses_the_sprite_edge() {
    // Sprite at x = 42: the block 40..44 is cut, its visible part (42, 43) shows the
    // sprite's own first column.
    let mut rig = sprite_rig(42, 0x1000, mosaic(1, 1, 4, 1));
    let f = rig.frame();
    assert_eq!(px(&f, 42, 30), sprite_texel(100, 0, 0));
    assert_eq!(px(&f, 43, 30), sprite_texel(100, 0, 0));
    assert_eq!(px(&f, 44, 30), sprite_texel(100, 2, 0));
    assert_eq!(px(&f, 47, 30), sprite_texel(100, 2, 0));
    assert_eq!(px(&f, 48, 30), sprite_texel(100, 6, 0));
}

#[test]
fn obj_mosaic_needs_the_sprite_bit_and_ignores_the_bg_sizes() {
    let mut off = sprite_rig(40, 0, mosaic(1, 1, 4, 3));
    let mut plain = sprite_rig(40, 0, 0);
    assert_eq!(off.frame(), plain.frame(), "attr0 bit 12 clear");
    let mut bg_only = sprite_rig(40, 0x1000, mosaic(8, 8, 1, 1));
    let mut plain2 = sprite_rig(40, 0x1000, 0);
    assert_eq!(
        bg_only.frame(),
        plain2.frame(),
        "BG sizes do not touch sprites"
    );
}

#[test]
fn obj_mosaic_applies_to_flipped_sprites_and_affine_sprites() {
    // H-flip: the sprite is mirrored first, then the blocks are taken in the
    // mirrored picture.
    let mut rig = sprite_rig(40, 0x1000, mosaic(1, 1, 4, 1));
    rig.oam(0, 30 | 0x1000, 40 | 1 << 14 | 0x1000, 100);
    let f = rig.frame();
    assert_eq!(px(&f, 41, 30), sprite_texel(100, 15, 0));
    assert_eq!(px(&f, 44, 30), sprite_texel(100, 11, 0));
    // Affine with the identity matrix looks like the regular sprite.
    let mut rig = sprite_rig(40, 0x1000, mosaic(1, 1, 4, 3));
    let regular = rig.frame();
    for (i, v) in [0x100i16, 0, 0, 0x100].into_iter().enumerate() {
        let o = 6 + i * 8;
        rig.ppu.oam[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }
    rig.oam(0, 30 | 0x1000 | 0x100, 40 | 1 << 14, 100);
    assert_eq!(rig.frame(), regular);
}

#[test]
fn mosaic_register_is_write_only() {
    let mut rig = Rig::new();
    rig.write(MOSAIC, 0xFFFF);
    assert_eq!(rig.ppu.read_io(MOSAIC), 0);
}
