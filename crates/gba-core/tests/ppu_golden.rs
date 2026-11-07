//! Golden-frame tests: one deterministic hand-built scene per BG mode, pinned by
//! the FNV-1a hash of the framebuffer (little-endian bytes). Set `GBA_PPU_DUMP=1`
//! to write the frames as PNGs into `target/gba-ppu/` for a visual check.
//!
//! The first six scenes use only the basic features (no windows, blending, mosaic, affine
//! BGs or sprites) and leave those registers at zero, so adding those features must
//! not change their hashes. The last three pin them.

mod common;

use common::{frame_hash, new_ppu, run_frame, set16, write_png};
use gba_core::ppu::Ppu;

const OBJ_VRAM: usize = 0x1_0000;

fn rgb(r: u16, g: u16, b: u16) -> u16 {
    (r & 31) | (g & 31) << 5 | (b & 31) << 10
}

/// Palette index of texel (r, c) of generated tile `n` (0..16, 0 transparent).
/// Eight patterns, picked by the tile number, in two colours derived from it.
fn texel(n: usize, r: usize, c: usize) -> u8 {
    let a = 1 + (n * 3) % 15;
    let b = 1 + (n * 5 + 7) % 15;
    let v = match n % 8 {
        0 => a,
        1 => {
            if r % 2 == 0 {
                a
            } else {
                b
            }
        }
        2 => {
            if c % 2 == 0 {
                a
            } else {
                b
            }
        }
        3 => {
            if (r ^ c) & 1 == 0 {
                a
            } else {
                b
            }
        }
        4 => [a, b, 0, b][(r + c) % 4],
        5 => {
            if r == 0 || c == 0 || r == 7 || c == 7 {
                a
            } else {
                b
            }
        }
        6 => {
            if r == c || r + c == 7 {
                a
            } else {
                0
            }
        }
        _ => {
            if r % 4 == 1 && c % 4 == 2 {
                a
            } else {
                0
            }
        }
    };
    v as u8
}

/// Backdrop dark blue; BG palette a colour ramp; OBJ palette another.
fn palettes(ppu: &mut Ppu) {
    for i in 0..256usize {
        let bg = rgb((i * 5) as u16, (i * 3 + 8) as u16, (i * 7 + 16) as u16);
        let obj = rgb((i * 11 + 4) as u16, (i * 13) as u16, (i * 3 + 20) as u16);
        set16(
            &mut ppu.palette[..],
            i * 2,
            if i == 0 { rgb(2, 2, 10) } else { bg },
        );
        set16(&mut ppu.palette[..], 0x200 + i * 2, obj);
    }
}

/// 4bpp generated tiles `0..count` at byte offset `base`.
fn tiles4(ppu: &mut Ppu, base: usize, count: usize) {
    for n in 0..count {
        for r in 0..8 {
            for c in 0..4 {
                let (lo, hi) = (texel(n, r, 2 * c), texel(n, r, 2 * c + 1));
                ppu.vram[base + n * 32 + r * 4 + c] = lo | hi << 4;
            }
        }
    }
}

/// 8bpp generated tiles: the 4bpp pattern shifted into one of eight 16-colour groups.
fn tiles8(ppu: &mut Ppu, base: usize, count: usize) {
    for n in 0..count {
        for r in 0..8 {
            for c in 0..8 {
                let t = texel(n, r, c);
                ppu.vram[base + n * 64 + r * 8 + c] =
                    if t == 0 { 0 } else { t + 16 * (n % 8) as u8 };
            }
        }
    }
}

/// Fill a `w` x `h` cell map (in 32x32 screen blocks starting at `base`) from `cell`.
fn map(ppu: &mut Ppu, base: usize, w: usize, h: usize, cell: impl Fn(usize, usize) -> u16) {
    for ty in 0..h {
        for tx in 0..w {
            let block = (tx / 32) + (ty / 32) * (w / 32);
            let at = base + block * 0x800 + ((ty % 32) * 32 + tx % 32) * 2;
            set16(&mut ppu.vram[..], at, cell(tx, ty));
        }
    }
}

/// A map entry for 4bpp tile `1 + k % 30` with hash-derived flips and bank.
fn entry4(tx: usize, ty: usize) -> u16 {
    let k = tx * 7 + ty * 13;
    (1 + k % 30) as u16 | ((k / 3 % 4) as u16) << 10 | (((tx + ty) % 16) as u16) << 12
}

fn reg(ppu: &mut Ppu, off: u32, v: u16) {
    ppu.write_io(off, v, 0xFFFF);
}

/// 24 sprites of mixed shapes, sizes, priorities, flips and palette banks. Tiles
/// start at `first_tile` (512 in bitmap modes).
fn sprites(ppu: &mut Ppu, first_tile: usize) {
    for i in 0..128usize {
        let o = i * 8;
        let (a0, a1, a2) = if i < 24 {
            let (shape, size) = (i % 3, (i / 3) % 4);
            let is8 = i % 7 == 0;
            let y = (i * 23) % 140;
            let x = (i * 37 + 11) % 260; // some wrap past the right edge
            let flips = [0u16, 0x1000, 0x2000, 0x3000, 0][i % 5];
            let tile = first_tile + (i * 29) % 300;
            (
                y as u16 | u16::from(is8) << 13 | (shape as u16) << 14,
                x as u16 | flips | (size as u16) << 14,
                tile as u16 | ((i % 4) as u16) << 10 | ((i % 16) as u16) << 12,
            )
        } else {
            (0x0200, 0, 0)
        };
        set16(&mut ppu.oam[..], o, a0);
        set16(&mut ppu.oam[..], o + 2, a1);
        set16(&mut ppu.oam[..], o + 4, a2);
    }
}

fn render(name: &str, ppu: &mut Ppu, sched: &mut gba_core::scheduler::Scheduler) -> u64 {
    run_frame(ppu, sched);
    if std::env::var_os("GBA_PPU_DUMP").is_some() {
        write_png(
            ppu.framebuffer(),
            &common::out_dir().join(format!("golden_{name}.png")),
        );
    }
    frame_hash(ppu.framebuffer())
}

fn text_scene(ppu: &mut Ppu, mode: u16) {
    palettes(ppu);
    tiles4(ppu, 0, 31);
    tiles8(ppu, 0x4000, 64);
    tiles4(ppu, OBJ_VRAM, 1024);
    // BG0: 4bpp, char 0, screen 16 (0x8000), 32x32, priority 3.
    map(ppu, 0x8000, 32, 32, entry4);
    reg(ppu, 0x008, 3 | 16 << 8);
    // BG1: 8bpp, char 1, screen 17 (0x8800), 64x32, priority 2, scrolled.
    map(ppu, 0x8800, 64, 32, |tx, ty| {
        (((tx * 5 + ty * 3) % 30) + 1) as u16
    });
    reg(ppu, 0x00A, 2 | 1 << 2 | 1 << 7 | 17 << 8 | 1 << 14);
    reg(ppu, 0x014, 37);
    reg(ppu, 0x016, 11);
    // BG2: 4bpp, char 0, screen 20 (0xA000), 64x64, priority 3, scrolled.
    map(ppu, 0xA000, 64, 64, |tx, ty| entry4(ty, tx));
    reg(ppu, 0x00C, 3 | 20 << 8 | 3 << 14);
    reg(ppu, 0x018, 301);
    reg(ppu, 0x01A, 450);
    // BG3: 4bpp, char 0, screen 31 (0xF800), 32x32, priority 1, mostly transparent (tile 31 is never written).
    map(ppu, 0xF800, 32, 32, |tx, ty| {
        if (tx + 2 * ty) % 5 == 0 {
            7 + ((tx + ty) % 3) as u16 * 8
        } else {
            31
        }
    });
    reg(ppu, 0x00E, 1 | 31 << 8);
    reg(ppu, 0x01C, 3);
    reg(ppu, 0x01E, 5);
    sprites(ppu, 1);
    let bgs = if mode == 0 { 0x0F00 } else { 0x0300 };
    // Mode 1 uses 2D mapping, mode 0 1D.
    reg(
        ppu,
        0x000,
        mode | bgs | 0x1000 | if mode == 0 { 0x40 } else { 0 },
    );
}

#[test]
fn golden_mode0_four_text_backgrounds_and_sprites() {
    let (mut ppu, mut sched) = new_ppu();
    text_scene(&mut ppu, 0);
    assert_eq!(render("mode0", &mut ppu, &mut sched), GOLDEN[0]);
}

#[test]
fn golden_mode1_two_text_backgrounds_and_sprites_2d() {
    let (mut ppu, mut sched) = new_ppu();
    text_scene(&mut ppu, 1);
    assert_eq!(render("mode1", &mut ppu, &mut sched), GOLDEN[1]);
}

#[test]
fn golden_mode2_sprites_only() {
    // Mode 2's BGs are affine (P2), so this scene enables sprites and no BG.
    let (mut ppu, mut sched) = new_ppu();
    palettes(&mut ppu);
    tiles4(&mut ppu, OBJ_VRAM, 1024);
    sprites(&mut ppu, 1);
    reg(&mut ppu, 0x000, 2 | 0x1000);
    assert_eq!(render("mode2", &mut ppu, &mut sched), GOLDEN[2]);
}

#[test]
fn golden_mode3_direct_colour_bitmap_and_sprites() {
    let (mut ppu, mut sched) = new_ppu();
    palettes(&mut ppu);
    tiles4(&mut ppu, OBJ_VRAM, 1024);
    for y in 0..160usize {
        for x in 0..240usize {
            let c = rgb((x / 8) as u16, (y / 5) as u16, ((x ^ y) & 31) as u16);
            set16(&mut ppu.vram[..], (y * 240 + x) * 2, c);
        }
    }
    sprites(&mut ppu, 512);
    // BG2 at priority 2, so sprites of priority 0-2 draw over it.
    reg(&mut ppu, 0x00C, 2);
    reg(&mut ppu, 0x000, 3 | 0x400 | 0x1000 | 0x40);
    assert_eq!(render("mode3", &mut ppu, &mut sched), GOLDEN[3]);
}

#[test]
fn golden_mode4_paletted_bitmap_second_page_and_sprites() {
    let (mut ppu, mut sched) = new_ppu();
    palettes(&mut ppu);
    tiles4(&mut ppu, OBJ_VRAM, 1024);
    for y in 0..160usize {
        for x in 0..240usize {
            // Page 0 is a decoy; page 1 (selected) has diagonal bands and a clear
            // (index 0, backdrop) hole in the middle.
            ppu.vram[y * 240 + x] = 99;
            let hole = (80..160).contains(&x) && (50..110).contains(&y);
            ppu.vram[0xA000 + y * 240 + x] = if hole {
                0
            } else {
                ((x + y) / 6 % 200 + 1) as u8
            };
        }
    }
    sprites(&mut ppu, 512);
    // BG2 at priority 2, so sprites of priority 0-2 draw over it.
    reg(&mut ppu, 0x00C, 2);
    reg(&mut ppu, 0x000, 4 | 0x10 | 0x400 | 0x1000);
    assert_eq!(render("mode4", &mut ppu, &mut sched), GOLDEN[4]);
}

#[test]
fn golden_mode5_small_bitmap_with_backdrop_border_and_sprites() {
    let (mut ppu, mut sched) = new_ppu();
    palettes(&mut ppu);
    tiles4(&mut ppu, OBJ_VRAM, 1024);
    for y in 0..128usize {
        for x in 0..160usize {
            let c = rgb(((x + y) / 4) as u16, (y / 4) as u16, (x / 5) as u16);
            set16(&mut ppu.vram[..], (y * 160 + x) * 2, c);
            set16(&mut ppu.vram[..], 0xA000 + (y * 160 + x) * 2, !c);
        }
    }
    sprites(&mut ppu, 512);
    // BG2 at priority 2, so sprites of priority 0-2 draw over it.
    reg(&mut ppu, 0x00C, 2);
    reg(&mut ppu, 0x000, 5 | 0x400 | 0x1000 | 0x40);
    assert_eq!(render("mode5", &mut ppu, &mut sched), GOLDEN[5]);
}

/// Write `v` to OAM halfword `half` of entry `index`.
fn oam16(ppu: &mut Ppu, index: usize, half: usize, v: u16) {
    set16(&mut ppu.oam[..], index * 8 + half * 2, v);
}

/// OR `bits` into attribute 0 of sprite `index`.
fn oam_attr0_or(ppu: &mut Ppu, index: usize, bits: u16) {
    let o = index * 8;
    let v = u16::from_le_bytes([ppu.oam[o], ppu.oam[o + 1]]) | bits;
    set16(&mut ppu.oam[..], o, v);
}

/// Rotation/scaling parameter group `g` (the fourth halfword of OAM entries 4g..4g+3).
fn oam_group(ppu: &mut Ppu, g: usize, m: [i16; 4]) {
    for (i, v) in m.into_iter().enumerate() {
        set16(&mut ppu.oam[..], g * 32 + 6 + i * 8, v as u16);
    }
}

#[test]
fn golden_mode2_affine_backgrounds_and_affine_sprites() {
    let (mut ppu, mut sched) = new_ppu();
    palettes(&mut ppu);
    // 8bpp tiles 0..62 (62 and 63 stay empty: transparent).
    tiles8(&mut ppu, 0, 62);
    tiles4(&mut ppu, OBJ_VRAM, 1024);
    // BG2: 512x512 (64x64 tiles) at screen block 2, wraparound on, priority 2,
    // rotated 30 degrees about the screen centre.
    for ty in 0..64usize {
        for tx in 0..64usize {
            ppu.vram[0x1000 + ty * 64 + tx] = ((tx * 5 + ty * 3) % 62 + 1) as u8;
        }
    }
    reg(&mut ppu, 0x00C, 2 | 2 << 8 | 1 << 13 | 2 << 14);
    for (i, v) in [222i16, -128, 128, 222].into_iter().enumerate() {
        reg(&mut ppu, 0x020 + 2 * i as u32, v as u16);
    }
    // Texel (256, 256) at the screen centre (120, 80).
    let x0 = 256 * 256 - (222 * 120 - 128 * 80);
    let y0 = 256 * 256 - (128 * 120 + 222 * 80);
    reg(&mut ppu, 0x028, x0 as u16);
    reg(&mut ppu, 0x02A, (x0 >> 16) as u16);
    reg(&mut ppu, 0x02C, y0 as u16);
    reg(&mut ppu, 0x02E, (y0 >> 16) as u16);
    // BG3: 128x128 (16x16 tiles) at screen block 6, no wrap, priority 1, zoomed out 2x,
    // mostly transparent (tile 63).
    for ty in 0..16usize {
        for tx in 0..16usize {
            ppu.vram[0x3000 + ty * 16 + tx] = if (tx + ty) % 3 == 0 {
                ((tx * 4 + ty) % 62 + 1) as u8
            } else {
                63
            };
        }
    }
    reg(&mut ppu, 0x00E, 1 | 6 << 8);
    for (i, v) in [0x200i16, 0, 0, 0x200].into_iter().enumerate() {
        reg(&mut ppu, 0x030 + 2 * i as u32, v as u16);
    }
    sprites(&mut ppu, 1);
    // Three double-size 32x32 affine sprites: 30 degree rotation, 2x zoom, 60 degrees
    // at a 3/4 scale, on parameter groups 0, 1, 2.
    oam_group(&mut ppu, 0, [222, -128, 128, 222]);
    oam_group(&mut ppu, 1, [0x80, 0, 0, 0x80]);
    oam_group(&mut ppu, 2, [96, -166, 166, 96]);
    for (k, (x, y)) in [(20u16, 90u16), (100, 20), (170, 96)]
        .into_iter()
        .enumerate()
    {
        let i = 120 + k;
        oam16(&mut ppu, i, 0, y | 0x300);
        oam16(&mut ppu, i, 1, x | (k as u16) << 9 | 2 << 14);
        oam16(&mut ppu, i, 2, (40 + k * 17) as u16 | (k as u16) << 12);
    }
    // 2D mapping (DISPCNT bit 6 clear), BG2 + BG3 + OBJ.
    reg(&mut ppu, 0x000, 2 | 0x0C00 | 0x1000);
    assert_eq!(render("mode2_affine", &mut ppu, &mut sched), GOLDEN[6]);
}

/// Mode 0 scene (as `text_scene`) under WIN0, WIN1 and an OBJ window with alpha blending.
#[test]
fn golden_mode0_windows_and_alpha_blending() {
    let (mut ppu, mut sched) = new_ppu();
    text_scene(&mut ppu, 0);
    // Every sixth sprite is semi-transparent.
    for i in (0..24).step_by(6) {
        oam_attr0_or(&mut ppu, i, 1 << 10);
    }
    // An OBJ-window sprite (mode 2, 64x64, tile 5) at (150, 8).
    oam16(&mut ppu, 126, 0, 8 | 2 << 10);
    oam16(&mut ppu, 126, 1, 150 | 3 << 14);
    oam16(&mut ppu, 126, 2, 5);
    // WIN0 x 30..130 y 20..100: BG0, BG1, BG3, OBJ, effects.
    reg(&mut ppu, 0x040, 30 << 8 | 130);
    reg(&mut ppu, 0x044, 20 << 8 | 100);
    // WIN1 x 110..220 y 60..140: BG1, BG2 only, no effects.
    reg(&mut ppu, 0x042, 110 << 8 | 220);
    reg(&mut ppu, 0x046, 60 << 8 | 140);
    reg(&mut ppu, 0x048, 0x3B | 0x06 << 8);
    // Outside: BG0, BG1, BG3, OBJ and effects; OBJ window: BG2 and OBJ, effects.
    reg(&mut ppu, 0x04A, 0x3B | 0x34 << 8);
    // Alpha: first BG1 | BG3 | OBJ, second BG0 | BG2 | backdrop, EVA 9, EVB 7.
    reg(
        &mut ppu,
        0x050,
        0x02 | 0x08 | 0x10 | 1 << 6 | (0x01 | 0x04 | 0x20) << 8,
    );
    reg(&mut ppu, 0x052, 9 | 7 << 8);
    reg(&mut ppu, 0x000, 0x0F00 | 0x1000 | 0x40 | 0xE000);
    assert_eq!(
        render("mode0_window_blend", &mut ppu, &mut sched),
        GOLDEN[7]
    );
}

#[test]
fn golden_mode0_bg_and_obj_mosaic() {
    let (mut ppu, mut sched) = new_ppu();
    text_scene(&mut ppu, 0);
    // BG0 and BG2 take part in mosaic, BG1 and BG3 do not; half the sprites do.
    reg(&mut ppu, 0x008, 3 | 16 << 8 | 1 << 6);
    reg(&mut ppu, 0x00C, 3 | 20 << 8 | 3 << 14 | 1 << 6);
    for i in (0..24).step_by(2) {
        oam_attr0_or(&mut ppu, i, 1 << 12);
    }
    // BG 5x4, OBJ 3x2 (the register holds size - 1).
    reg(&mut ppu, 0x04C, 4 | 3 << 4 | 2 << 8 | 1 << 12);
    assert_eq!(render("mode0_mosaic", &mut ppu, &mut sched), GOLDEN[8]);
}

/// Pinned after checking the PNGs. What each shows (240x160, 2x scaled PNGs):
/// 0. four text BGs (4bpp 32x32, 8bpp 64x32 scrolled, 4bpp 64x64 scrolled, sparse
///    BG3 in front) with 1D-mapped sprites;
/// 1. BG0 and BG1 only, with 2D-mapped sprites;
/// 2. sprites only on the dark-blue backdrop (affine BGs are P2);
/// 3. an RGB gradient with a checker (mode 3), sprites using tiles 512+;
/// 4. diagonal colour bands from page 1 (page 0 is a decoy) around a backdrop hole;
/// 5. a 160x128 gradient (page 0) with the backdrop to the right and below;
/// 6. (P2) mode 2: a 512x512 wrapping affine BG2 rotated 30 degrees, a zoomed-out
///    sparse BG3 in front, and three double-size affine sprites;
/// 7. (P2) the mode 0 scene under WIN0, WIN1 and an OBJ window with alpha blending
///    and semi-transparent sprites;
/// 8. (P2) the mode 0 scene with BG0/BG2 mosaic (5x4) and OBJ mosaic (3x2).
const GOLDEN: [u64; 9] = [
    0x0aa0448b2beba7aa,
    0xe6b20d59ec66d383,
    0xa66b48f4bce9452c,
    0xf8afc72b9c76adfb,
    0xa223c6968e134494,
    0x3bb8f624b318c130,
    0xf1378c6038df51ef,
    0x8046baf87e1d7b12,
    0xec85c4d085ff35b0,
];
