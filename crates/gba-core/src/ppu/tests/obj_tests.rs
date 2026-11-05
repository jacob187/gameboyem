//! Sprite tests.

use super::*;

const OBJ_ON: u16 = 0x1000;
const ONE_D: u16 = 0x0040;
const HFLIP: u16 = 0x1000;
const VFLIP: u16 = 0x2000;
const OBJ_VRAM: usize = 0x1_0000;

fn attr0(y: u16, shape: u16, is8: bool) -> u16 {
    y | u16::from(is8) << 13 | shape << 14
}

fn attr1(x: u16, size: u16, flips: u16) -> u16 {
    (x & 0x1FF) | flips | size << 14
}

fn attr2(tile: u16, priority: u16, bank: u16) -> u16 {
    tile | priority << 10 | bank << 12
}

/// Fill every 4bpp OBJ tile `n` with the solid palette index `1 + n % 15`.
fn solid_obj_tiles(rig: &mut Rig) {
    for n in 0..1024usize {
        let idx = (1 + n % 15) as u8;
        rig.ppu.vram[OBJ_VRAM + n * 32..OBJ_VRAM + n * 32 + 32].fill(idx | idx << 4);
    }
}

fn obj_colour(idx: usize) -> u16 {
    pal_colour(256 + idx)
}

const SHAPES: [(u16, u16, usize, usize); 12] = [
    (0, 0, 8, 8),
    (0, 1, 16, 16),
    (0, 2, 32, 32),
    (0, 3, 64, 64),
    (1, 0, 16, 8),
    (1, 1, 32, 8),
    (1, 2, 32, 16),
    (1, 3, 64, 32),
    (2, 0, 8, 16),
    (2, 1, 8, 32),
    (2, 2, 16, 32),
    (2, 3, 32, 64),
];

#[test]
fn all_twelve_shapes_cover_exactly_their_box_in_1d_and_2d() {
    for one_d in [false, true] {
        for (shape, size, w, h) in SHAPES {
            let mut rig = Rig::new();
            rig.distinct_palette();
            solid_obj_tiles(&mut rig);
            rig.hide_all_sprites();
            let (x0, y0, base) = (10usize, 20usize, 3usize);
            rig.oam(
                0,
                attr0(y0 as u16, shape, false),
                attr1(x0 as u16, size, 0),
                attr2(base as u16, 0, 0),
            );
            rig.write(DISPCNT, OBJ_ON | if one_d { ONE_D } else { 0 });
            let f = rig.frame();
            let backdrop = pal_colour(0);
            let covered = f.iter().filter(|&&p| p != backdrop).count();
            assert_eq!(covered, w * h, "shape {shape} size {size} 1d={one_d}");
            for py in 0..h {
                for px_ in 0..w {
                    let tile = if one_d {
                        base + (py / 8) * (w / 8) + px_ / 8
                    } else {
                        base + (py / 8) * 32 + px_ / 8
                    };
                    assert_eq!(
                        px(&f, x0 + px_, y0 + py),
                        obj_colour(1 + tile % 15),
                        "shape {shape} size {size} 1d={one_d} local {px_},{py}"
                    );
                }
            }
        }
    }
}

#[test]
fn eight_bpp_sprites_use_two_tile_slots_and_ignore_the_bank() {
    for one_d in [false, true] {
        let mut rig = Rig::new();
        rig.distinct_palette();
        rig.hide_all_sprites();
        // 8bpp tile `t` (even) occupies slots t and t+1: fill it with 1 + (t / 2) % 250.
        for t in (0..1024usize).step_by(2) {
            let a = OBJ_VRAM + t * 32;
            rig.ppu.vram[a..a + 64].fill((1 + (t / 2) % 250) as u8);
        }
        rig.oam(0, attr0(0, 0, true), attr1(0, 1, 0), attr2(4, 0, 9));
        rig.write(DISPCNT, OBJ_ON | if one_d { ONE_D } else { 0 });
        let f = rig.frame();
        let tiles = if one_d { [4, 6, 8, 10] } else { [4, 6, 36, 38] };
        for (cell, t) in tiles.iter().enumerate() {
            let (cx, cy) = ((cell % 2) * 8, (cell / 2) * 8);
            assert_eq!(
                px(&f, cx + 3, cy + 5),
                obj_colour(1 + (t / 2) % 250),
                "1d={one_d} cell {cell}"
            );
        }
    }
}

/// A sprite's 16x16 4bpp tiles: tile `n` has pixel (r, c) = `1 + (r * 3 + c + n) % 15`.
fn patterned_tiles(rig: &mut Rig) {
    for n in 0..1024usize {
        for r in 0..8 {
            for c in 0..4 {
                let lo = 1 + (r * 3 + 2 * c + n) % 15;
                let hi = 1 + (r * 3 + 2 * c + 1 + n) % 15;
                rig.ppu.vram[OBJ_VRAM + n * 32 + r * 4 + c] = (lo | hi << 4) as u8;
            }
        }
    }
}

#[test]
fn sprite_flips_mirror_the_whole_sprite() {
    for (flips, name) in [
        (0, "none"),
        (HFLIP, "h"),
        (VFLIP, "v"),
        (HFLIP | VFLIP, "hv"),
    ] {
        let mut rig = Rig::new();
        rig.distinct_palette();
        rig.hide_all_sprites();
        patterned_tiles(&mut rig);
        let base = 100;
        rig.oam(
            0,
            attr0(30, 0, false),
            attr1(40, 1, flips),
            attr2(base as u16, 0, 2),
        );
        rig.write(DISPCNT, OBJ_ON | ONE_D);
        let f = rig.frame();
        for py in 0..16usize {
            for px_ in 0..16usize {
                let sx = if flips & HFLIP != 0 { 15 - px_ } else { px_ };
                let sy = if flips & VFLIP != 0 { 15 - py } else { py };
                let tile = base + (sy / 8) * 2 + sx / 8;
                let idx = 1 + ((sy % 8) * 3 + sx % 8 + tile) % 15;
                assert_eq!(
                    px(&f, 40 + px_, 30 + py),
                    obj_colour(2 * 16 + idx),
                    "{name} {px_},{py}"
                );
            }
        }
    }
}

#[test]
fn sprite_x_wraps_at_512_and_clips_at_the_screen_edges() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    solid_obj_tiles(&mut rig);
    rig.hide_all_sprites();
    // x = 508 is -4: the right half of an 8-wide sprite shows at x 0..4.
    rig.oam(0, attr0(0, 0, false), attr1(508, 0, 0), attr2(1, 0, 0));
    // x = 239 shows one column; x = 240 shows none.
    rig.oam(1, attr0(20, 0, false), attr1(239, 0, 0), attr2(2, 0, 0));
    rig.oam(2, attr0(40, 0, false), attr1(240, 0, 0), attr2(3, 0, 0));
    // x = 511 is -1.
    rig.oam(3, attr0(60, 0, false), attr1(511, 0, 0), attr2(4, 0, 0));
    rig.write(DISPCNT, OBJ_ON | ONE_D);
    let f = rig.frame();
    let b = pal_colour(0);
    assert_eq!(px(&f, 0, 0), obj_colour(2));
    assert_eq!(px(&f, 3, 0), obj_colour(2));
    assert_eq!(px(&f, 4, 0), b);
    assert_eq!(px(&f, 239, 20), obj_colour(3));
    assert_eq!(px(&f, 238, 20), b);
    assert_eq!(px(&f, 239, 40), b);
    assert_eq!(px(&f, 0, 60), obj_colour(5));
    assert_eq!(px(&f, 6, 60), obj_colour(5));
    assert_eq!(px(&f, 7, 60), b);
}

#[test]
fn sprite_y_wraps_at_256() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    rig.hide_all_sprites();
    patterned_tiles(&mut rig);
    // 8x16 sprite (shape 2, size 0) at y = 250: lines 250..256 then 0..10.
    rig.oam(0, attr0(250, 2, false), attr1(0, 0, 0), attr2(7, 0, 0));
    rig.write(DISPCNT, OBJ_ON | ONE_D);
    let f = rig.frame();
    let idx_at = |sy: usize, sx: usize| {
        let tile = 7 + sy / 8;
        obj_colour(1 + ((sy % 8) * 3 + sx + tile) % 15)
    };
    // Line 0 is the sprite's row 6; line 9 its row 15; line 10 nothing.
    assert_eq!(px(&f, 2, 0), idx_at(6, 2));
    assert_eq!(px(&f, 5, 9), idx_at(15, 5));
    assert_eq!(px(&f, 5, 10), pal_colour(0));
    // The part below the wrap: lines 250.. are off screen.
    assert_eq!(px(&f, 5, 159), pal_colour(0));
}

#[test]
fn sprite_appears_on_exactly_its_lines() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    solid_obj_tiles(&mut rig);
    rig.hide_all_sprites();
    rig.oam(0, attr0(100, 0, false), attr1(0, 0, 0), attr2(1, 0, 0));
    rig.write(DISPCNT, OBJ_ON | ONE_D);
    let f = rig.frame();
    assert_eq!(px(&f, 0, 99), pal_colour(0));
    assert_eq!(px(&f, 0, 100), obj_colour(2));
    assert_eq!(px(&f, 7, 107), obj_colour(2));
    assert_eq!(px(&f, 0, 108), pal_colour(0));
    assert_eq!(px(&f, 8, 100), pal_colour(0));
}

#[test]
fn sprite_priority_against_sprites() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    solid_obj_tiles(&mut rig);
    rig.hide_all_sprites();
    // Overlapping 8x8 sprites at x = 0 (A, B) and a third, lower priority number, at x = 4.
    rig.oam(0, attr0(0, 0, false), attr1(0, 0, 0), attr2(1, 2, 0)); // idx 2, prio 2
    rig.oam(1, attr0(0, 0, false), attr1(0, 0, 0), attr2(2, 2, 0)); // idx 3, prio 2
    rig.oam(2, attr0(0, 0, false), attr1(4, 0, 0), attr2(3, 1, 0)); // idx 4, prio 1
    rig.oam(3, attr0(0, 0, false), attr1(4, 0, 0), attr2(4, 1, 0)); // idx 5, prio 1
    rig.write(DISPCNT, OBJ_ON | ONE_D);
    let f = rig.frame();
    // Equal priority: the lower OAM index wins.
    assert_eq!(px(&f, 0, 0), obj_colour(2));
    // Lower priority number beats a lower OAM index; among those, the lower index.
    assert_eq!(px(&f, 5, 0), obj_colour(4));
    assert_eq!(px(&f, 11, 0), obj_colour(4));
}

#[test]
fn transparent_sprite_pixels_do_not_occlude() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    rig.hide_all_sprites();
    // Sprite 0: tile 1 with only its left column opaque; sprite 1 (tile 2) is solid below it.
    for r in 0..8 {
        rig.ppu.vram[OBJ_VRAM + 32 + r * 4] = 0x01;
    }
    rig.ppu.vram[OBJ_VRAM + 64..OBJ_VRAM + 96].fill(0x33);
    rig.oam(0, attr0(0, 0, false), attr1(0, 0, 0), attr2(1, 0, 0));
    rig.oam(1, attr0(0, 0, false), attr1(0, 0, 0), attr2(2, 3, 0));
    rig.write(DISPCNT, OBJ_ON | ONE_D);
    let f = rig.frame();
    assert_eq!(px(&f, 0, 3), obj_colour(1));
    assert_eq!(px(&f, 1, 3), obj_colour(3));
}

#[test]
fn sprite_vs_background_priority_obj_wins_ties() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    solid_obj_tiles(&mut rig);
    rig.hide_all_sprites();
    solid_bg(&mut rig, 0, 100, 1);
    solid_bg(&mut rig, 1, 101, 2);
    // Sprite at (0,0) priority 1 (tile 1: idx 2); at (16,0) priority 2; at (32,0) priority 0;
    // at (48,0) priority 3.
    for (i, (x, prio)) in [(0u16, 1u16), (16, 2), (32, 0), (48, 3)]
        .into_iter()
        .enumerate()
    {
        rig.oam(i, attr0(0, 0, false), attr1(x, 0, 0), attr2(1, prio, 0));
    }
    rig.write(DISPCNT, OBJ_ON | ONE_D | bg_on(0) | bg_on(1));
    let f = rig.frame();
    // Same priority as BG0: the sprite is in front.
    assert_eq!(px(&f, 0, 0), obj_colour(2));
    // Priority 2 loses to BG0 (priority 1).
    assert_eq!(px(&f, 16, 0), pal_colour(100));
    assert_eq!(px(&f, 32, 0), obj_colour(2));
    assert_eq!(px(&f, 48, 0), pal_colour(100));
    // Elsewhere the BGs show.
    assert_eq!(px(&f, 100, 0), pal_colour(100));
    // Sprite priority 2 against BG1 at priority 2 only: it wins the tie.
    rig.write(0x008, 3 | 1 << 7 | 8 << 8); // BG0 down to priority 3
    let f = rig.frame();
    assert_eq!(px(&f, 16, 0), obj_colour(2));
    assert_eq!(px(&f, 48, 0), pal_colour(101));
}

#[test]
fn disabled_and_prohibited_sprites_are_not_drawn() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    solid_obj_tiles(&mut rig);
    rig.hide_all_sprites();
    rig.oam(
        0,
        attr0(0, 0, false) | 0x200,
        attr1(0, 0, 0),
        attr2(1, 0, 0),
    );
    // Shape 3 is prohibited.
    rig.oam(2, attr0(0, 3, false), attr1(32, 0, 0), attr2(1, 0, 0));
    // Mode 3 (attr0 bits 10-11 = 3) is prohibited.
    rig.oam(
        3,
        attr0(0, 0, false) | 0xC00,
        attr1(48, 0, 0),
        attr2(1, 0, 0),
    );
    // Mode 2 (OBJ window) draws no colour.
    rig.oam(
        4,
        attr0(0, 0, false) | 0x800,
        attr1(64, 0, 0),
        attr2(1, 0, 0),
    );
    // A normal one for contrast.
    rig.oam(5, attr0(0, 0, false), attr1(80, 0, 0), attr2(1, 0, 0));
    rig.write(DISPCNT, OBJ_ON | ONE_D);
    let f = rig.frame();
    for x in [0, 32, 48, 64] {
        assert_eq!(px(&f, x, 0), pal_colour(0), "x {x}");
    }
    assert_eq!(px(&f, 80, 0), obj_colour(2));
}

#[test]
fn obj_enable_bit_hides_sprites() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    solid_obj_tiles(&mut rig);
    rig.hide_all_sprites();
    rig.oam(0, attr0(0, 0, false), attr1(0, 0, 0), attr2(1, 0, 0));
    rig.write(DISPCNT, ONE_D);
    assert_eq!(px(&rig.frame(), 0, 0), pal_colour(0));
    rig.write(DISPCNT, ONE_D | OBJ_ON);
    assert_eq!(px(&rig.frame(), 0, 0), obj_colour(2));
}

#[test]
fn bitmap_modes_only_show_obj_tiles_512_and_up() {
    for mode in [3u16, 4, 5] {
        let mut rig = Rig::new();
        rig.distinct_palette();
        solid_obj_tiles(&mut rig);
        rig.hide_all_sprites();
        rig.oam(0, attr0(0, 0, false), attr1(0, 0, 0), attr2(511, 0, 0));
        rig.oam(1, attr0(0, 0, false), attr1(16, 0, 0), attr2(512, 0, 0));
        // A 16x8 sprite starting at tile 510 in 1D: its second tile (511) is hidden too,
        // so use tiles 511 and 512: the left tile is hidden, the right one shows.
        rig.oam(2, attr0(16, 1, false), attr1(0, 0, 0), attr2(511, 0, 0));
        rig.write(DISPCNT, mode | OBJ_ON | ONE_D);
        let f = rig.frame();
        let b = px(&f, 100, 100);
        assert_eq!(px(&f, 0, 0), b, "mode {mode}: tile 511");
        assert_eq!(
            px(&f, 16, 0),
            obj_colour(1 + 512 % 15),
            "mode {mode}: tile 512"
        );
        assert_eq!(px(&f, 0, 16), b, "mode {mode}: left tile 511");
        assert_eq!(
            px(&f, 8, 16),
            obj_colour(1 + 512 % 15),
            "mode {mode}: right tile 512"
        );
    }
    // In mode 0 tile 511 is shown.
    let mut rig = Rig::new();
    rig.distinct_palette();
    solid_obj_tiles(&mut rig);
    rig.hide_all_sprites();
    rig.oam(0, attr0(0, 0, false), attr1(0, 0, 0), attr2(511, 0, 0));
    rig.write(DISPCNT, OBJ_ON | ONE_D);
    assert_eq!(px(&rig.frame(), 0, 0), obj_colour(1 + 511 % 15));
}

#[test]
fn sprites_draw_over_a_bitmap_background() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    solid_obj_tiles(&mut rig);
    rig.hide_all_sprites();
    rig.vram16(0, rgb(31, 0, 0));
    rig.oam(0, attr0(0, 0, false), attr1(1, 0, 0), attr2(601, 0, 0));
    rig.write(DISPCNT, 3 | 0x400 | OBJ_ON | ONE_D);
    let f = rig.frame();
    assert_eq!(px(&f, 0, 0), rgb(31, 0, 0));
    assert_eq!(px(&f, 1, 0), obj_colour(1 + 601 % 15));
}

#[test]
fn obj_tile_index_wraps_in_the_32k_obj_area() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    solid_obj_tiles(&mut rig);
    rig.hide_all_sprites();
    // 1D 16x8 sprite at tile 1023: the second tile wraps to tile 0.
    rig.oam(0, attr0(0, 1, false), attr1(0, 0, 0), attr2(1023, 0, 0));
    rig.write(DISPCNT, OBJ_ON | ONE_D);
    let f = rig.frame();
    assert_eq!(px(&f, 0, 0), obj_colour(1 + 1023 % 15));
    assert_eq!(px(&f, 8, 0), obj_colour(1));
}

// ---- Affine (rotation/scaling) sprites ----

const AFFINE: u16 = 0x100;
const DOUBLE: u16 = 0x200;

/// Parameter group `g`: PA..PD are the fourth halfword of OAM entries 4g..4g+3.
fn set_group(rig: &mut Rig, g: usize, [pa, pb, pc, pd]: [i16; 4]) {
    for (i, v) in [pa, pb, pc, pd].into_iter().enumerate() {
        let o = g * 32 + 6 + i * 8;
        rig.ppu.oam[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }
}

/// attr1 of an affine sprite: x, size and the parameter group in bits 9-13.
fn affine_attr1(x: u16, size: u16, group: u16) -> u16 {
    (x & 0x1FF) | group << 9 | size << 14
}

/// Colour of texel (u, v) of a 16x16 4bpp [`patterned_tiles`] sprite whose first tile
/// is `base`.
fn texel16(base: usize, one_d: bool, u: usize, v: usize) -> u16 {
    let n = base
        + if one_d {
            (v / 8) * 2 + u / 8
        } else {
            (v / 8) * 32 + u / 8
        };
    obj_colour(1 + ((v % 8) * 3 + (u % 8) + n) % 15)
}

/// A rig with one 16x16 affine sprite (OAM 0) at (40, 30), tiles from 100, group 0.
fn affine_rig(one_d: bool, double: bool, matrix: [i16; 4]) -> Rig {
    let mut rig = Rig::new();
    rig.distinct_palette();
    rig.hide_all_sprites();
    patterned_tiles(&mut rig);
    set_group(&mut rig, 0, matrix);
    let a0 = attr0(30, 0, false) | AFFINE | if double { DOUBLE } else { 0 };
    rig.oam(0, a0, affine_attr1(40, 1, 0), attr2(100, 0, 0));
    rig.write(DISPCNT, OBJ_ON | if one_d { ONE_D } else { 0 });
    rig
}

#[test]
fn affine_identity_matches_the_plain_sprite_in_1d_and_2d() {
    for one_d in [false, true] {
        let mut rig = affine_rig(one_d, false, [0x100, 0, 0, 0x100]);
        let f = rig.frame();
        for (u, v) in [(0, 0), (7, 7), (8, 0), (15, 15), (3, 12), (12, 4)] {
            assert_eq!(
                px(&f, 40 + u, 30 + v),
                texel16(100, one_d, u, v),
                "1d={one_d} ({u},{v})"
            );
        }
        assert_eq!(px(&f, 56, 30), pal_colour(0), "right of the box");
        assert_eq!(px(&f, 40, 46), pal_colour(0), "below the box");
        assert_eq!(px(&f, 39, 30), pal_colour(0), "left of the box");
    }
}

#[test]
fn affine_zoom_in_is_clipped_to_the_box_without_double_size() {
    // PA = PD = 0x80: each box pixel steps half a texel, the sprite shows its centre
    // 8x8 texels at 2x, and nothing outside the 16x16 box.
    let mut rig = affine_rig(true, false, [0x80, 0, 0, 0x80]);
    let f = rig.frame();
    for (ix, iy) in [(0, 0), (1, 1), (5, 9), (15, 15), (8, 8)] {
        assert_eq!(
            px(&f, 40 + ix, 30 + iy),
            texel16(100, true, 4 + ix / 2, 4 + iy / 2),
            "({ix},{iy})"
        );
    }
    assert_eq!(px(&f, 56, 38), pal_colour(0));
    assert_eq!(px(&f, 48, 46), pal_colour(0));
}

#[test]
fn double_size_doubles_the_box_so_the_whole_sprite_shows() {
    // Identity with double size: the 16x16 image sits centred in a 32x32 box.
    let mut rig = affine_rig(true, true, [0x100, 0, 0, 0x100]);
    let f = rig.frame();
    assert_eq!(px(&f, 40, 30), pal_colour(0), "box corner is empty");
    assert_eq!(
        px(&f, 40 + 7, 30 + 8),
        pal_colour(0),
        "just left of the image"
    );
    assert_eq!(px(&f, 40 + 8, 30 + 8), texel16(100, true, 0, 0));
    assert_eq!(px(&f, 40 + 23, 30 + 23), texel16(100, true, 15, 15));
    assert_eq!(
        px(&f, 40 + 24, 30 + 8),
        pal_colour(0),
        "just right of the image"
    );
    // At 2x zoom the image fills the whole 32x32 box.
    set_group(&mut rig, 0, [0x80, 0, 0, 0x80]);
    let f = rig.frame();
    for (ix, iy) in [(0, 0), (31, 31), (17, 5), (9, 22)] {
        assert_eq!(
            px(&f, 40 + ix, 30 + iy),
            texel16(100, true, ix / 2, iy / 2),
            "({ix},{iy})"
        );
    }
    assert_eq!(px(&f, 72, 30), pal_colour(0), "past the box");
    assert_eq!(px(&f, 40, 62), pal_colour(0), "below the box");
}

#[test]
fn affine_swapped_matrix_transposes() {
    for double in [false, true] {
        // u = y, v = x (about the centre).
        let mut rig = affine_rig(false, double, [0, 0x100, 0x100, 0]);
        let f = rig.frame();
        let off = if double { 8 } else { 0 };
        for (u, v) in [(0, 0), (3, 9), (15, 1), (12, 12)] {
            // Box pixel (ix, iy) = (v + off, u + off) shows texel (iy - off, ix - off).
            assert_eq!(
                px(&f, 40 + v + off, 30 + u + off),
                texel16(100, false, u, v),
                "double={double} ({u},{v})"
            );
        }
    }
}

#[test]
fn rotated_sprites_are_cut_by_a_normal_box_but_not_by_a_double_one() {
    // About 45 degrees: PA = PD = 181, PB = -181, PC = 181 (0xB5 / 256 ~ 0.707).
    let m = [181, -181, 181, 181];
    let count = |f: &[u16], size: usize| {
        let mut n = 0;
        for y in 0..size {
            for x in 0..size {
                n += usize::from(px(f, 40 + x, 30 + y) != pal_colour(0));
            }
        }
        n
    };
    let normal = affine_rig(true, false, m).frame();
    let double = affine_rig(true, true, m).frame();
    let (n, d) = (count(&normal, 16), count(&double, 32));
    // The rotated 16x16 square covers about 256 pixels, the normal box clips it.
    assert!(d > n + 40, "double {d} vs normal {n}");
    // The corners of the double box are empty, the centre is not.
    assert_eq!(px(&double, 40, 30), pal_colour(0));
    assert_eq!(px(&double, 71, 30), pal_colour(0));
    assert_ne!(px(&double, 56, 46), pal_colour(0));
    // The normal box loses the corner the rotation pushes outside the image.
    assert_eq!(px(&normal, 40 + 15, 30), pal_colour(0));
}

#[test]
fn affine_parameter_groups_are_independent_and_group_31_is_the_last_oam_slot() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    rig.hide_all_sprites();
    patterned_tiles(&mut rig);
    set_group(&mut rig, 0, [0x100, 0, 0, 0x100]);
    set_group(&mut rig, 31, [0x80, 0, 0, 0x80]);
    rig.oam(
        0,
        attr0(0, 0, false) | AFFINE,
        affine_attr1(0, 1, 0),
        attr2(100, 0, 0),
    );
    rig.oam(
        1,
        attr0(0, 0, false) | AFFINE,
        affine_attr1(32, 1, 31),
        attr2(100, 0, 0),
    );
    // Entry 127's attr3 (OAM 0x3FE) is group 31's PD: the hide_all fill did not
    // break it because set_group wrote after.
    rig.write(DISPCNT, OBJ_ON | ONE_D);
    let f = rig.frame();
    assert_eq!(px(&f, 5, 3), texel16(100, true, 5, 3));
    assert_eq!(px(&f, 32 + 5, 3), texel16(100, true, 4 + 5 / 2, 4 + 3 / 2));
    assert_eq!(rig.ppu.oam[0x3FE..0x400], 0x80i16.to_le_bytes());
}

#[test]
fn affine_eight_bpp_sprite_uses_the_matrix() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    rig.hide_all_sprites();
    // 16x16 8bpp: 4 tiles of 2 slots; fill tile slot pair `t` with index 1 + t / 2.
    for t in (0..64usize).step_by(2) {
        let a = OBJ_VRAM + (10 + t) * 32;
        rig.ppu.vram[a..a + 64].fill((1 + t / 2) as u8);
    }
    set_group(&mut rig, 3, [0x100, 0, 0, 0x100]);
    rig.oam(
        0,
        attr0(0, 0, true) | AFFINE,
        affine_attr1(0, 1, 3),
        attr2(10, 0, 5),
    );
    rig.write(DISPCNT, OBJ_ON | ONE_D);
    let f = rig.frame();
    // 1D: tiles of a row are consecutive, 2 slots each: (0,0)=slot 10, (8,0)=12,
    // (0,8)=14, (8,8)=16 -> indices 1, 2, 3, 4 (the palette bank is ignored).
    assert_eq!(px(&f, 2, 2), obj_colour(1));
    assert_eq!(px(&f, 10, 2), obj_colour(2));
    assert_eq!(px(&f, 2, 10), obj_colour(3));
    assert_eq!(px(&f, 10, 10), obj_colour(4));
}

#[test]
fn affine_sprites_clip_at_the_left_edge_and_wrap_in_y() {
    let mut rig = affine_rig(true, false, [0x100, 0, 0, 0x100]);
    // x = -10 (9-bit 502): screen column 0 is texel column 10.
    rig.oam(
        0,
        attr0(30, 0, false) | AFFINE,
        affine_attr1(502, 1, 0),
        attr2(100, 0, 0),
    );
    let f = rig.frame();
    assert_eq!(px(&f, 0, 30), texel16(100, true, 10, 0));
    assert_eq!(px(&f, 5, 33), texel16(100, true, 15, 3));
    assert_eq!(px(&f, 6, 30), pal_colour(0));
    // A double-size box at y = 250 spans lines 250..256 and 0..26.
    set_group(&mut rig, 0, [0x100, 0, 0, 0x100]);
    rig.oam(
        0,
        attr0(250, 0, false) | AFFINE | DOUBLE,
        affine_attr1(100, 1, 0),
        attr2(100, 0, 0),
    );
    let f = rig.frame();
    // Box row 14..24 holds the image: screen line 14 - 6 = 8 is box row 14, texel row 6.
    assert_eq!(px(&f, 100 + 8, 8), texel16(100, true, 0, 6));
    assert_eq!(
        px(&f, 100 + 8, 25),
        pal_colour(0),
        "box row 31 is below the image"
    );
    assert_eq!(px(&f, 108, 27), pal_colour(0));
}

#[test]
fn affine_sprites_obey_priority_and_the_bitmap_tile_rule() {
    let mut rig = affine_rig(true, false, [0x100, 0, 0, 0x100]);
    // A regular sprite of better priority on top of the affine one.
    solid_obj_tiles(&mut rig);
    rig.oam(1, attr0(30, 0, false), attr1(40, 0, 0), attr2(1, 0, 0));
    rig.oam(
        0,
        attr0(30, 0, false) | AFFINE,
        affine_attr1(40, 1, 0),
        attr2(100, 1, 0),
    );
    let f = rig.frame();
    assert_eq!(px(&f, 40, 30), obj_colour(2), "priority 0 sprite wins");
    assert_ne!(px(&f, 48, 30), obj_colour(2));
    // Mode 3: tiles below 512 are not shown, affine or not.
    rig.write(DISPCNT, 3 | OBJ_ON | ONE_D);
    let f = rig.frame();
    assert_eq!(
        px(&f, 48, 38),
        pal_colour(0),
        "tile 100 hidden in a bitmap mode"
    );
}
