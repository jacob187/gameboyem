//! Sprites (OBJ). GBATEK "LCD OBJ - OAM Attributes", "LCD OBJ - Overview".
//!
//! P1 draws regular (non-affine) sprites. The affine bits are parsed (see
//! [`Sprite`]) so P2 can add rotation/scaling in [`render_line`] without touching
//! the callers.

use super::render::{Mem, OBJ_EMPTY, OBJ_SEMI, OBJ_WINDOW, Regs, TRANSPARENT};
use crate::SCREEN_WIDTH;

/// OBJ tile memory (the upper 32 KiB of VRAM).
const OBJ_TILES: usize = 0x1_0000;

/// Width and height in pixels, indexed by `[shape][size]` (attr0 bits 14-15, attr1
/// bits 14-15). Shape 3 is prohibited.
const DIMENSIONS: [[(u32, u32); 4]; 3] = [
    [(8, 8), (16, 16), (32, 32), (64, 64)],
    [(16, 8), (32, 8), (32, 16), (64, 32)],
    [(8, 16), (8, 32), (16, 32), (32, 64)],
];

/// How a sprite pixel is drawn (attr0 bits 10-11).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ObjMode {
    Normal,
    /// Semi-transparent: alpha-blended with the layer below (P2).
    SemiTransparent,
    /// OBJ window: draws nothing, marks the OBJ window (P2).
    Window,
}

/// A decoded OAM entry.
struct Sprite {
    /// Top edge (attr0 bits 0-7, wraps at 256).
    y: u32,
    /// Left edge, signed 9 bits (attr1 bits 0-8): 256..511 are -256..-1.
    x: i32,
    width: u32,
    height: u32,
    /// attr0 bit 8: rotation/scaling sprite. P2 draws these.
    affine: bool,
    /// attr0 bit 9 on a non-affine sprite: not displayed. On an affine sprite it
    /// is the double-size flag: the sprite's bounding box is twice `width` x
    /// `height` and the displayed area is that bigger box (P2).
    double_or_disabled: bool,
    mode: ObjMode,
    is8: bool,
    /// attr1 bits 12-13 (non-affine only; on affine sprites bits 9-13 select the
    /// rotation/scaling parameter group, `(attr1 >> 9) & 31`).
    hflip: bool,
    vflip: bool,
    tile: usize,
    priority: u32,
    palette_bank: usize,
}

impl Sprite {
    /// Decode the OAM entry `index`; `None` for the prohibited shape / mode.
    fn read(mem: &Mem, index: usize) -> Option<Self> {
        let o = index * 8;
        let attr = |i: usize| u16::from_le_bytes([mem.oam[o + i], mem.oam[o + i + 1]]);
        let (a0, a1, a2) = (attr(0), attr(2), attr(4));
        let shape = usize::from(a0 >> 14);
        if shape == 3 {
            return None;
        }
        let mode = match (a0 >> 10) & 3 {
            0 => ObjMode::Normal,
            1 => ObjMode::SemiTransparent,
            2 => ObjMode::Window,
            _ => return None,
        };
        let (width, height) = DIMENSIONS[shape][usize::from(a1 >> 14)];
        let x = i32::from(a1 & 0x1FF);
        Some(Self {
            y: u32::from(a0 & 0xFF),
            x: if x >= 256 { x - 512 } else { x },
            width,
            height,
            affine: a0 & 0x100 != 0,
            double_or_disabled: a0 & 0x200 != 0,
            mode,
            is8: a0 & 0x2000 != 0,
            hflip: a1 & 0x1000 != 0,
            vflip: a1 & 0x2000 != 0,
            tile: usize::from(a2 & 0x3FF),
            priority: u32::from((a2 >> 10) & 3),
            palette_bank: usize::from(a2 >> 12),
        })
    }
}

/// Draw the sprites on line `y` into `out` (cleared first); returns whether any
/// sprite pixel was drawn.
///
/// Sprites are visited in OAM order; a pixel is replaced only by a sprite of a
/// strictly lower priority number, so among equal priorities the lower OAM index
/// wins. The per-line cycle budget (1210 cycles of sprite fetch) is not modelled.
pub(super) fn render_line(regs: &Regs, mem: &Mem, y: u32, out: &mut [u32; SCREEN_WIDTH]) -> bool {
    out.fill(OBJ_EMPTY);
    let one_d = regs.dispcnt & 0x40 != 0;
    // In the bitmap modes 3-5 the upper half of the BG bitmap overlaps OBJ tiles
    // 0..512, which are therefore not displayed (GBATEK "LCD OBJ - OAM Attributes").
    let bitmap = regs.mode() >= 3;
    let mut drawn = false;
    for index in 0..128 {
        let Some(s) = Sprite::read(mem, index) else {
            continue;
        };
        if s.affine || s.double_or_disabled {
            // Affine sprites are drawn by P2; with the bit set on a non-affine
            // sprite it is disabled.
            continue;
        }
        // Y wraps at 256: a sprite at y=250 with height 16 covers lines 250..256, 0..10.
        let dy = y.wrapping_sub(s.y) & 0xFF;
        if dy >= s.height {
            continue;
        }
        if s.x >= SCREEN_WIDTH as i32 || s.x + s.width as i32 <= 0 {
            continue;
        }
        let row = if s.vflip { s.height - 1 - dy } else { dy };
        let (tile_row, fine_y) = ((row >> 3) as usize, (row & 7) as usize);
        let tiles_wide = (s.width >> 3) as usize;
        // An 8bpp tile is two 32-byte units; tile numbers always count 32-byte units.
        let step = if s.is8 { 2 } else { 1 };
        let row_bytes = if s.is8 { 8 } else { 4 };
        for column in 0..tiles_wide {
            let screen_x = s.x + (column * 8) as i32;
            if screen_x >= SCREEN_WIDTH as i32 || screen_x + 8 <= 0 {
                continue;
            }
            let src = if s.hflip {
                tiles_wide - 1 - column
            } else {
                column
            };
            // 1D: tiles of a sprite are consecutive. 2D: the tile memory is a 32-tile
            // wide grid (one row = 1024 bytes).
            let tile = if one_d {
                s.tile + (tile_row * tiles_wide + src) * step
            } else {
                s.tile + tile_row * 32 + src * step
            } & 0x3FF;
            if bitmap && tile < 512 {
                continue;
            }
            let addr = OBJ_TILES + ((tile * 32 + fine_y * row_bytes) & 0x7FFF);
            let texels = mem.tile_row(addr, s.is8, s.hflip);
            let lo = (-screen_x).max(0) as usize;
            let hi = (SCREEN_WIDTH as i32 - screen_x).min(8) as usize;
            for (i, &idx) in texels.iter().enumerate().take(hi).skip(lo) {
                if idx == 0 {
                    continue;
                }
                let slot = &mut out[(screen_x + i as i32) as usize];
                if s.mode == ObjMode::Window {
                    *slot |= OBJ_WINDOW;
                    continue;
                }
                let current = *slot;
                if current & u32::from(TRANSPARENT) == 0 && s.priority >= (current >> 16) & 3 {
                    continue;
                }
                let pal = if s.is8 {
                    256 + usize::from(idx)
                } else {
                    256 + s.palette_bank * 16 + usize::from(idx)
                };
                let semi = if s.mode == ObjMode::SemiTransparent {
                    OBJ_SEMI
                } else {
                    0
                };
                *slot =
                    (current & OBJ_WINDOW) | u32::from(mem.colour(pal)) | s.priority << 16 | semi;
                drawn = true;
            }
        }
    }
    drawn
}
