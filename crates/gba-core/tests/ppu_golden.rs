//! Golden-frame tests: one deterministic hand-built scene per BG mode, pinned by
//! the FNV-1a hash of the framebuffer (little-endian bytes). Set `GBA_PPU_DUMP=1`
//! to write the frames as PNGs into `target/gba-ppu/` for a visual check.
//!
//! The scenes use only P1 features (no windows, blending, mosaic, affine BGs or
//! sprites) and leave those registers at zero, so P2 must not change any hash.

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

/// Pinned after checking the PNGs. What each shows (240x160, 2x scaled PNGs):
/// 0. four text BGs (4bpp 32x32, 8bpp 64x32 scrolled, 4bpp 64x64 scrolled, sparse
///    BG3 in front) with 1D-mapped sprites;
/// 1. BG0 and BG1 only, with 2D-mapped sprites;
/// 2. sprites only on the dark-blue backdrop (affine BGs are P2);
/// 3. an RGB gradient with a checker (mode 3), sprites using tiles 512+;
/// 4. diagonal colour bands from page 1 (page 0 is a decoy) around a backdrop hole;
/// 5. a 160x128 gradient (page 0) with the backdrop to the right and below.
const GOLDEN: [u64; 6] = [
    0x0aa0448b2beba7aa,
    0xe6b20d59ec66d383,
    0xa66b48f4bce9452c,
    0xf8afc72b9c76adfb,
    0xa223c6968e134494,
    0x3bb8f624b318c130,
];
