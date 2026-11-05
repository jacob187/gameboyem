//! Affine (rotation/scaling) background tests: BG2 in modes 1-5, BG3 in mode 2.

use super::*;

const BG2PA: u32 = 0x020;
const BG2X: u32 = 0x028;
const BG2Y: u32 = 0x02C;
const BG3PA: u32 = 0x030;
const BG3X: u32 = 0x038;
const BG2CNT: u32 = 0x00C;
const BG3CNT: u32 = 0x00E;
const WRAP: u16 = 0x2000;

fn set_matrix(rig: &mut Rig, pa_reg: u32, [pa, pb, pc, pd]: [i16; 4]) {
    for (i, v) in [pa, pb, pc, pd].into_iter().enumerate() {
        rig.write(pa_reg + 2 * i as u32, v as u16);
    }
}

/// Write a 28-bit reference point register (two halfwords, 20.8 fixed point).
fn set_ref(rig: &mut Rig, reg: u32, v: i32) {
    rig.write(reg, v as u16);
    rig.write(reg + 2, (v >> 16) as u16 & 0x0FFF);
}

/// Map entry (8bpp affine maps are one byte per tile) at tile (tx, ty).
fn map_tile(tx: usize, ty: usize) -> u8 {
    (1 + (tx * 3 + ty * 7) % 200) as u8
}

/// Texel (r, c) of 8bpp tile `n`: distinct enough that a wrong fetch shows.
fn texel8(n: usize, r: usize, c: usize) -> u8 {
    (1 + (n * 5 + r * 8 + c) % 250) as u8
}

/// The BG pixel at texel (u, v) of a map `tiles` tiles wide.
fn expect(u: usize, v: usize) -> u8 {
    texel8(usize::from(map_tile(u / 8, v / 8)), v % 8, u % 8)
}

/// An affine BG `bg` (2 or 3) with char base 0, screen base 8 (0x4000), the given
/// size (0..=3, 16..128 tiles square) and flags in BGxCNT; tile data and map are
/// the patterns above.
fn affine_bg(rig: &mut Rig, bg: usize, size: u16, flags: u16) {
    rig.distinct_palette();
    for n in 0..256usize {
        for r in 0..8 {
            for c in 0..8 {
                rig.ppu.vram[n * 64 + r * 8 + c] = texel8(n, r, c);
            }
        }
    }
    let tiles = 16usize << size;
    for ty in 0..tiles {
        for tx in 0..tiles {
            rig.ppu.vram[0x4000 + ty * tiles + tx] = map_tile(tx, ty);
        }
    }
    rig.write(0x008 + 2 * bg as u32, 8 << 8 | size << 14 | flags);
    let pa = if bg == 2 { BG2PA } else { BG3PA };
    set_matrix(rig, pa, [0x100, 0, 0, 0x100]);
}

fn colour_of(idx: u8) -> u16 {
    pal_colour(usize::from(idx))
}

#[test]
fn identity_matrix_shows_the_map_and_outside_is_transparent_without_wrap() {
    let mut rig = Rig::new();
    affine_bg(&mut rig, 2, 0, 0);
    rig.pal(0, rgb(31, 0, 31));
    rig.write(DISPCNT, 2 | bg_on(2));
    let f = rig.frame();
    for (x, y) in [(0, 0), (7, 7), (8, 0), (50, 33), (127, 127), (100, 5)] {
        assert_eq!(px(&f, x, y), colour_of(expect(x, y)), "({x},{y})");
    }
    // The map is 128x128: past it nothing draws, in x and in y.
    assert_eq!(px(&f, 128, 0), rgb(31, 0, 31));
    assert_eq!(px(&f, 239, 10), rgb(31, 0, 31));
    assert_eq!(px(&f, 5, 128), rgb(31, 0, 31));
}

#[test]
fn scaling_stretches_and_shrinks() {
    // PA = PD = 0x80 steps half a texel per pixel: 2x zoom.
    let mut rig = Rig::new();
    affine_bg(&mut rig, 2, 0, 0);
    set_matrix(&mut rig, BG2PA, [0x80, 0, 0, 0x80]);
    rig.write(DISPCNT, 2 | bg_on(2));
    let f = rig.frame();
    for (x, y) in [(0, 0), (1, 1), (13, 29), (100, 90), (255 / 2, 60)] {
        assert_eq!(
            px(&f, x, y),
            colour_of(expect(x / 2, y / 2)),
            "zoom ({x},{y})"
        );
    }
    // PA = PD = 0x200: texels two apart, the map ends at screen x 63.
    set_matrix(&mut rig, BG2PA, [0x200, 0, 0, 0x200]);
    let f = rig.frame();
    for (x, y) in [(0, 0), (1, 1), (13, 29), (63, 63)] {
        assert_eq!(
            px(&f, x, y),
            colour_of(expect(x * 2, y * 2)),
            "shrink ({x},{y})"
        );
    }
    assert_eq!(px(&f, 64, 0), pal_colour(0));
    assert_eq!(px(&f, 0, 64), pal_colour(0));
}

#[test]
fn a_swapped_matrix_transposes_and_the_reference_point_offsets() {
    // u = y, v = x: the picture is mirrored along the diagonal.
    let mut rig = Rig::new();
    affine_bg(&mut rig, 2, 0, 0);
    set_matrix(&mut rig, BG2PA, [0, 0x100, 0x100, 0]);
    rig.write(DISPCNT, 2 | bg_on(2));
    let f = rig.frame();
    for (x, y) in [(0, 0), (5, 9), (31, 2), (100, 60)] {
        assert_eq!(px(&f, x, y), colour_of(expect(y, x)), "({x},{y})");
    }
    // Move the origin: texel (u, v) = (x + 16, y + 8).
    set_matrix(&mut rig, BG2PA, [0x100, 0, 0, 0x100]);
    set_ref(&mut rig, BG2X, 16 << 8);
    set_ref(&mut rig, BG2Y, 8 << 8);
    let f = rig.frame();
    for (x, y) in [(0, 0), (20, 30), (111, 119)] {
        assert_eq!(px(&f, x, y), colour_of(expect(x + 16, y + 8)), "({x},{y})");
    }
    assert_eq!(px(&f, 112, 0), pal_colour(0), "u = 128 is outside");
    assert_eq!(px(&f, 0, 120), pal_colour(0), "v = 128 is outside");
}

#[test]
fn wraparound_repeats_the_map_and_handles_negative_reference_points() {
    let mut rig = Rig::new();
    affine_bg(&mut rig, 2, 0, WRAP);
    rig.write(DISPCNT, 2 | bg_on(2));
    let f = rig.frame();
    for (x, y) in [(0, 0), (127, 0), (128, 0), (200, 3), (239, 159), (5, 130)] {
        assert_eq!(
            px(&f, x, y),
            colour_of(expect(x % 128, y % 128)),
            "({x},{y})"
        );
    }
    // A negative reference point (28-bit two's complement): u = x - 8, v = y - 24.
    set_ref(&mut rig, BG2X, -(8 << 8));
    set_ref(&mut rig, BG2Y, -(24 << 8));
    let f = rig.frame();
    for (x, y) in [(0, 0), (3, 10), (8, 24), (200, 100)] {
        let u = (x as i32 - 8).rem_euclid(128) as usize;
        let v = (y as i32 - 24).rem_euclid(128) as usize;
        assert_eq!(px(&f, x, y), colour_of(expect(u, v)), "wrap ({x},{y})");
    }
    // The same offsets without wraparound leave the negative part transparent.
    rig.write(BG2CNT, 8 << 8);
    let f = rig.frame();
    assert_eq!(px(&f, 0, 40), pal_colour(0), "u = -8");
    assert_eq!(px(&f, 20, 5), pal_colour(0), "v = -19");
    assert_eq!(px(&f, 8, 24), colour_of(expect(0, 0)));
}

#[test]
fn all_four_map_sizes_and_their_edges() {
    for size in 0..4u16 {
        let dim = 128usize << size;
        for wrap in [false, true] {
            let mut rig = Rig::new();
            affine_bg(&mut rig, 2, size, if wrap { WRAP } else { 0 });
            // Texel u = dim - 4 at screen x 0, so x = 3 is the last column and x = 4
            // is one past the edge.
            set_ref(&mut rig, BG2X, ((dim - 4) << 8) as i32);
            rig.write(DISPCNT, 2 | bg_on(2));
            let f = rig.frame();
            assert_eq!(
                px(&f, 3, 2),
                colour_of(expect(dim - 1, 2)),
                "size {size} edge"
            );
            let past = if wrap {
                colour_of(expect(0, 2))
            } else {
                pal_colour(0)
            };
            assert_eq!(px(&f, 4, 2), past, "size {size} wrap {wrap}");
        }
    }
}

#[test]
fn the_reference_point_advances_by_pb_and_pd_each_line() {
    // PB = 0x100 shears: u = x + y. PD = 0x80: v = y / 2.
    let mut rig = Rig::new();
    affine_bg(&mut rig, 2, 0, WRAP);
    set_matrix(&mut rig, BG2PA, [0x100, 0x100, 0, 0x80]);
    rig.write(DISPCNT, 2 | bg_on(2));
    let f = rig.frame();
    for (x, y) in [(0, 0), (10, 1), (10, 2), (33, 77), (200, 159)] {
        assert_eq!(
            px(&f, x, y),
            colour_of(expect((x + y) % 128, (y / 2) % 128)),
            "({x},{y})"
        );
    }
    // The next frame starts again from the registers (reload at VBlank), so it is
    // identical.
    assert_eq!(rig.frame(), f);
}

#[test]
fn writing_the_reference_point_mid_frame_takes_effect_at_once_and_per_axis() {
    // Identity: v = y. At line 10, move only X to 24: Y keeps counting.
    let mut rig = Rig::new();
    affine_bg(&mut rig, 2, 0, WRAP);
    rig.write(DISPCNT, 2 | bg_on(2));
    rig.run_to_line(0, 0);
    // Past the previous frame's VBlank reload and before line 10 is drawn.
    rig.run_to_line(10, 100);
    set_ref(&mut rig, BG2X, 24 << 8);
    let f = rig.frame();
    assert_eq!(px(&f, 4, 9), colour_of(expect(4, 9)));
    assert_eq!(px(&f, 4, 10), colour_of(expect(28, 10)), "new X on line 10");
    assert_eq!(px(&f, 4, 11), colour_of(expect(28, 11)), "Y kept counting");
    // The next frame reloads from the registers: BG2X is 24, BG2Y 0.
    let f = rig.frame();
    assert_eq!(px(&f, 4, 0), colour_of(expect(28, 0)));
    assert_eq!(px(&f, 4, 20), colour_of(expect(28, 20)));
}

#[test]
fn a_write_to_the_reference_point_during_vblank_is_latched_for_line_0() {
    let mut rig = Rig::new();
    affine_bg(&mut rig, 2, 0, WRAP);
    rig.write(DISPCNT, 2 | bg_on(2));
    let _ = rig.frame();
    // Now inside VBlank (line 160).
    set_ref(&mut rig, BG2Y, 40 << 8);
    let f = rig.frame();
    assert_eq!(px(&f, 0, 0), colour_of(expect(0, 40)));
    assert_eq!(px(&f, 0, 5), colour_of(expect(0, 45)));
}

#[test]
fn bg3_in_mode_2_has_its_own_matrix_and_mode_2_has_no_text_bgs() {
    let mut rig = Rig::new();
    // BG0 and BG1 are enabled in the DISPCNT but do not exist in mode 2. (Their
    // memory overlaps the affine map, so the affine BG is built over it.)
    solid_bg(&mut rig, 0, 40, 0);
    solid_bg(&mut rig, 1, 41, 0);
    affine_bg(&mut rig, 3, 0, 0);
    set_matrix(&mut rig, BG3PA, [0x200, 0, 0, 0x200]);
    set_ref(&mut rig, BG3X, 8 << 8);
    rig.write(DISPCNT, 2 | bg_on(0) | bg_on(1) | bg_on(3));
    let f = rig.frame();
    for (x, y) in [(0, 0), (3, 5), (30, 60)] {
        assert_eq!(
            px(&f, x, y),
            colour_of(expect(x * 2 + 8, y * 2)),
            "({x},{y})"
        );
    }
}

#[test]
fn mode_2_priorities_order_bg2_and_bg3() {
    let mut rig = Rig::new();
    affine_bg(&mut rig, 2, 0, 0);
    // BG3 shares the maps (screen 8) but is shifted by 8 texels.
    rig.write(BG3CNT, 8 << 8);
    set_matrix(&mut rig, BG3PA, [0x100, 0, 0, 0x100]);
    set_ref(&mut rig, BG3X, 8 << 8);
    rig.write(DISPCNT, 2 | bg_on(2) | bg_on(3));
    // Equal priority: BG2 in front.
    assert_eq!(px(&rig.frame(), 10, 10), colour_of(expect(10, 10)));
    // BG3 at priority 0 wins.
    rig.write(BG3CNT, 8 << 8);
    rig.write(BG2CNT, 8 << 8 | 1);
    assert_eq!(px(&rig.frame(), 10, 10), colour_of(expect(18, 10)));
}

#[test]
fn bg2_is_affine_in_mode_1_beside_two_text_bgs() {
    let mut rig = Rig::new();
    affine_bg(&mut rig, 2, 0, 0);
    rig.write(BG2CNT, 8 << 8 | 2);
    solid_bg(&mut rig, 1, 31, 1);
    rig.write(DISPCNT, 1 | bg_on(1) | bg_on(2));
    let f = rig.frame();
    // The text BG (priority 1) beats the affine BG (priority 2).
    assert_eq!(px(&f, 10, 10), pal_colour(31));
    rig.write(DISPCNT, 1 | bg_on(2));
    assert_eq!(px(&rig.frame(), 10, 10), colour_of(expect(10, 10)));
}

#[test]
fn bitmap_modes_use_the_matrix_and_never_wrap() {
    // Mode 3 at 2x shrink: texel (2x, 2y); beyond u = 240 (x = 120) transparent.
    let mut rig = Rig::new();
    rig.pal(0, rgb(1, 2, 3));
    for y in 0..160usize {
        for x in 0..240usize {
            rig.vram16((y * 240 + x) * 2, ((x * 3 + y * 5) & 0x7FFF) as u16);
        }
    }
    set_matrix(&mut rig, BG2PA, [0x200, 0, 0, 0x200]);
    rig.write(DISPCNT, 3 | bg_on(2));
    let f = rig.frame();
    assert_eq!(px(&f, 10, 20), ((20 * 3 + 40 * 5) & 0x7FFF) as u16);
    assert_eq!(px(&f, 119, 79), ((238 * 3 + 158 * 5) & 0x7FFF) as u16);
    assert_eq!(px(&f, 120, 10), rgb(1, 2, 3));
    assert_eq!(px(&f, 10, 80), rgb(1, 2, 3));
    // Mode 4 rotated 90 degrees (u = y, v = x) draws only where u < 240, v < 160,
    // and index 0 stays transparent.
    let mut rig = Rig::new();
    rig.distinct_palette();
    for y in 0..160usize {
        for x in 0..240usize {
            rig.ppu.vram[y * 240 + x] = (x % 7 + y % 11) as u8;
        }
    }
    set_matrix(&mut rig, BG2PA, [0, 0x100, 0x100, 0]);
    rig.write(DISPCNT, 4 | bg_on(2));
    let f = rig.frame();
    assert_eq!(px(&f, 30, 50), colour_of((50 % 7 + 30 % 11) as u8));
    assert_eq!(px(&f, 0, 0), pal_colour(0), "index 0 is transparent");
    assert_eq!(px(&f, 170, 5), pal_colour(0), "v = 170 is outside");
    // Mode 5 with a reference point shifts the 160x128 bitmap.
    let mut rig = Rig::new();
    rig.pal(0, rgb(9, 9, 9));
    for y in 0..128usize {
        for x in 0..160usize {
            rig.vram16((y * 160 + x) * 2, (x + y * 160 + 1) as u16);
        }
    }
    set_ref(&mut rig, BG2X, 20 << 8);
    rig.write(DISPCNT, 5 | bg_on(2));
    let f = rig.frame();
    assert_eq!(px(&f, 0, 0), 21);
    assert_eq!(px(&f, 139, 3), (159 + 3 * 160 + 1) as u16);
    assert_eq!(px(&f, 140, 3), rgb(9, 9, 9));
}

#[test]
fn the_last_tile_of_char_base_3_ends_at_the_64k_boundary() {
    let mut rig = Rig::new();
    rig.distinct_palette();
    // Char base 3 (0xC000) tile 255 ends exactly at 64 KiB, the edge of BG tile
    // memory: it still draws.
    rig.ppu.vram[0xC000 + 255 * 64..0x1_0000].fill(77);
    rig.ppu.vram[0x4000] = 255;
    rig.write(BG2CNT, 8 << 8 | 3 << 2);
    set_matrix(&mut rig, BG2PA, [0x100, 0, 0, 0x100]);
    rig.write(DISPCNT, 2 | bg_on(2));
    assert_eq!(px(&rig.frame(), 3, 3), pal_colour(77));
}

#[test]
fn the_reference_point_stays_a_28_bit_signed_value_as_it_advances() {
    // BG2X just below +2^27 with PB = +0x100: the second line's point wraps to
    // -2^27 (code-review P2#3).
    let mut rig = Rig::new();
    rig.write(DISPCNT, 2);
    set_matrix(&mut rig, BG2PA, [0x100, 0x100, 0, 0x100]);
    set_ref(&mut rig, BG2X, 0x07FF_FF00);
    assert_eq!(rig.ppu.affine_latch[0][0], 0x07FF_FF00);
    rig.run_to_line(0, 1100);
    assert_eq!(rig.ppu.affine_latch[0][0], -0x0800_0000);
    rig.run_to_line(1, 1100);
    assert_eq!(rig.ppu.affine_latch[0][0], -0x07FF_FF00);
    // And the other way: -2^27 with PB = -0x100 wraps to +0x07FF_FF00.
    let mut rig = Rig::new();
    rig.write(DISPCNT, 2);
    set_matrix(&mut rig, BG2PA, [0x100, -0x100, 0, 0x100]);
    set_ref(&mut rig, BG2X, -0x0800_0000);
    rig.run_to_line(0, 1100);
    assert_eq!(rig.ppu.affine_latch[0][0], 0x07FF_FF00);
}
