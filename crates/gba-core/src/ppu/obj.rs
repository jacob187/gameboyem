//! Sprites (OBJ). GBATEK "LCD OBJ - OAM Attributes", "LCD OBJ - OAM Rotation/Scaling
//! Parameters", "LCD OBJ - Overview", "LCD I/O Mosaic Function".

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

/// What a line's sprites left in the OBJ buffer.
#[derive(Clone, Copy, Default)]
pub(super) struct ObjLine {
    /// Some sprite pixel (colour or OBJ-window mark) is on the line. When false the
    /// buffer is entirely [`OBJ_EMPTY`].
    pub touched: bool,
    /// A semi-transparent sprite pixel was drawn.
    pub semi: bool,
}

/// How a sprite pixel is drawn (attr0 bits 10-11).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ObjMode {
    Normal,
    /// Semi-transparent: alpha-blended with the layer below.
    SemiTransparent,
    /// OBJ window: draws nothing, marks the OBJ window.
    Window,
}

/// A decoded OAM entry.
struct Sprite {
    /// Left edge, signed 9 bits (attr1 bits 0-8): 256..511 are -256..-1.
    x: i32,
    /// The sprite's own size; an affine sprite with the double-size flag has a
    /// bounding box twice as big in both directions (`box_w` x `box_h`).
    width: u32,
    height: u32,
    box_w: u32,
    box_h: u32,
    /// attr0 bit 8: rotation/scaling sprite.
    affine: bool,
    mode: ObjMode,
    is8: bool,
    /// attr1 bits 12-13 on a non-affine sprite. On an affine sprite bits 9-13 select
    /// the rotation/scaling parameter group instead (see `group`).
    hflip: bool,
    vflip: bool,
    /// Rotation/scaling parameter group (attr1 bits 9-13, affine sprites).
    group: usize,
    /// attr0 bit 12: the sprite takes part in OBJ mosaic.
    mosaic: bool,
    tile: usize,
    priority: u32,
    palette_bank: usize,
}

/// Per-line drawing context shared by the sprite paths.
struct Ctx<'a> {
    mem: &'a Mem<'a>,
    one_d: bool,
    /// In the bitmap modes 3-5 the upper half of the BG bitmap overlaps OBJ tiles
    /// 0..512, which are therefore not displayed (GBATEK "LCD OBJ - OAM Attributes").
    bitmap: bool,
    /// OBJ mosaic is not 1x1 (MOSAIC bits 8-15).
    mosaic_on: bool,
    /// OBJ mosaic horizontal size, and the screen line modulo the vertical size.
    mosaic_h: u32,
    mosaic_v_back: u32,
}

impl Sprite {
    /// Decode the OAM entry at byte offset `o`, given its already-read attributes 0
    /// and 1 and its size; `None` for the prohibited mode.
    fn decode(
        mem: &Mem,
        o: usize,
        (a0, a1): (u16, u16),
        (width, height): (u32, u32),
    ) -> Option<Self> {
        let a2 = u16::from_le_bytes([mem.oam[o + 4], mem.oam[o + 5]]);
        let mode = match (a0 >> 10) & 3 {
            0 => ObjMode::Normal,
            1 => ObjMode::SemiTransparent,
            2 => ObjMode::Window,
            _ => return None,
        };
        let x = i32::from(a1 & 0x1FF);
        let affine = a0 & 0x100 != 0;
        let double = affine && a0 & 0x200 != 0;
        let grow = u32::from(double);
        Some(Self {
            x: if x >= 256 { x - 512 } else { x },
            width,
            height,
            box_w: width << grow,
            box_h: height << grow,
            affine,
            mode,
            is8: a0 & 0x2000 != 0,
            hflip: a1 & 0x1000 != 0,
            vflip: a1 & 0x2000 != 0,
            group: usize::from((a1 >> 9) & 31),
            mosaic: a0 & 0x1000 != 0,
            tile: usize::from(a2 & 0x3FF),
            priority: u32::from((a2 >> 10) & 3),
            palette_bank: usize::from(a2 >> 12),
        })
    }

    /// Palette index (0 = transparent) of the texel at (`lx`, `ly`) of the sprite's
    /// own image, found through the tile mapping.
    fn texel(&self, ctx: &Ctx, lx: u32, ly: u32) -> u8 {
        let (tile_col, tile_row) = ((lx >> 3) as usize, (ly >> 3) as usize);
        let (fx, fy) = ((lx & 7) as usize, (ly & 7) as usize);
        let tiles_wide = (self.width >> 3) as usize;
        // An 8bpp tile is two 32-byte units; tile numbers always count 32-byte units.
        let step = if self.is8 { 2 } else { 1 };
        // 1D: tiles of a sprite are consecutive. 2D: the tile memory is a 32-tile
        // wide grid (one row = 1024 bytes).
        let tile = if ctx.one_d {
            self.tile + (tile_row * tiles_wide + tile_col) * step
        } else {
            self.tile + tile_row * 32 + tile_col * step
        } & 0x3FF;
        if ctx.bitmap && tile < 512 {
            return 0;
        }
        if self.is8 {
            ctx.mem.vram[OBJ_TILES + ((tile * 32 + fy * 8 + fx) & 0x7FFF)]
        } else {
            let b = ctx.mem.vram[OBJ_TILES + ((tile * 32 + fy * 4 + fx / 2) & 0x7FFF)];
            if fx & 1 == 0 { b & 0xF } else { b >> 4 }
        }
    }

    /// Put palette index `idx` at screen column `x` of the OBJ line.
    #[inline]
    fn plot(
        &self,
        mem: &Mem,
        out: &mut [u32; SCREEN_WIDTH],
        x: usize,
        idx: u8,
        line: &mut ObjLine,
    ) {
        if idx == 0 {
            return;
        }
        line.touched = true;
        let slot = &mut out[x];
        if self.mode == ObjMode::Window {
            *slot |= OBJ_WINDOW;
            return;
        }
        let current = *slot;
        if current & u32::from(TRANSPARENT) == 0 && self.priority >= (current >> 16) & 3 {
            return;
        }
        let pal = if self.is8 {
            256 + usize::from(idx)
        } else {
            256 + self.palette_bank * 16 + usize::from(idx)
        };
        let semi = if self.mode == ObjMode::SemiTransparent {
            line.semi = true;
            OBJ_SEMI
        } else {
            0
        };
        *slot = (current & OBJ_WINDOW) | u32::from(mem.colour(pal)) | self.priority << 16 | semi;
    }
}

/// Draw the sprites on line `y` into `out` (cleared first).
///
/// Sprites are visited in OAM order; a pixel is replaced only by a sprite of a
/// strictly lower priority number, so among equal priorities the lower OAM index
/// wins. The per-line cycle budget (1210 cycles of sprite fetch) is not modelled.
pub(super) fn render_line(
    regs: &Regs,
    mem: &Mem,
    y: u32,
    out: &mut [u32; SCREEN_WIDTH],
) -> ObjLine {
    out.fill(OBJ_EMPTY);
    let (mosaic_h, mosaic_v) = regs.obj_mosaic();
    let ctx = Ctx {
        mem,
        one_d: regs.dispcnt & 0x40 != 0,
        bitmap: regs.mode() >= 3,
        mosaic_on: mosaic_h > 1 || mosaic_v > 1,
        mosaic_h,
        mosaic_v_back: y % mosaic_v,
    };
    let mut line = ObjLine::default();
    for index in 0..128 {
        // Reject on attributes 0 and 1 alone (the Y range) before decoding the rest.
        let o = index * 8;
        let a0 = u16::from_le_bytes([mem.oam[o], mem.oam[o + 1]]);
        let affine = a0 & 0x100 != 0;
        let double = a0 & 0x200 != 0;
        if double && !affine {
            // Bit 9 on a non-affine sprite: not displayed.
            continue;
        }
        let shape = usize::from(a0 >> 14);
        if shape == 3 {
            continue;
        }
        let a1 = u16::from_le_bytes([mem.oam[o + 2], mem.oam[o + 3]]);
        let size = DIMENSIONS[shape][usize::from(a1 >> 14)];
        let box_h = size.1 << u32::from(double);
        // Y wraps at 256: a sprite at y=250 with height 16 covers lines 250..256, 0..10.
        let dy = y.wrapping_sub(u32::from(a0 & 0xFF)) & 0xFF;
        if dy >= box_h {
            continue;
        }
        let Some(s) = Sprite::decode(mem, o, (a0, a1), size) else {
            continue;
        };
        if s.x >= SCREEN_WIDTH as i32 || s.x + s.box_w as i32 <= 0 {
            continue;
        }
        if s.affine {
            draw_affine(&ctx, &s, dy, out, &mut line);
        } else if s.mosaic && ctx.mosaic_on {
            draw_regular_mosaic(&ctx, &s, dy, out, &mut line);
        } else {
            draw_regular(&ctx, &s, dy, out, &mut line);
        }
    }
    line
}

/// A regular sprite on its row `dy`, a tile row at a time.
fn draw_regular(ctx: &Ctx, s: &Sprite, dy: u32, out: &mut [u32; SCREEN_WIDTH], line: &mut ObjLine) {
    let mem = ctx.mem;
    let row = if s.vflip { s.height - 1 - dy } else { dy };
    let (tile_row, fine_y) = ((row >> 3) as usize, (row & 7) as usize);
    let tiles_wide = (s.width >> 3) as usize;
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
        let tile = if ctx.one_d {
            s.tile + (tile_row * tiles_wide + src) * step
        } else {
            s.tile + tile_row * 32 + src * step
        } & 0x3FF;
        if ctx.bitmap && tile < 512 {
            continue;
        }
        let addr = OBJ_TILES + ((tile * 32 + fine_y * row_bytes) & 0x7FFF);
        let texels = mem.tile_row(addr, s.is8, s.hflip);
        let lo = (-screen_x).max(0) as usize;
        let hi = (SCREEN_WIDTH as i32 - screen_x).min(8) as usize;
        for (i, &idx) in texels.iter().enumerate().take(hi).skip(lo) {
            s.plot(mem, out, (screen_x + i as i32) as usize, idx, line);
        }
    }
}

/// OBJ mosaic: the mosaic block is aligned to the screen (column 0, line 0) and every
/// pixel of a block shows the block's top-left pixel; where the block starts outside
/// the sprite, the sprite's own edge stands in. (GBATEK: "Normally, a 'mosaic-pixel'
/// is colorized by the color of the upperleft covered pixel.")
fn mosaic_pos(ctx: &Ctx, s: &Sprite, screen_x: i32, dy: u32) -> (u32, u32) {
    let block_x = screen_x - screen_x % ctx.mosaic_h as i32;
    let lx = (block_x - s.x).max(0) as u32;
    let ly = dy.saturating_sub(ctx.mosaic_v_back);
    (lx, ly)
}

fn draw_regular_mosaic(
    ctx: &Ctx,
    s: &Sprite,
    dy: u32,
    out: &mut [u32; SCREEN_WIDTH],
    line: &mut ObjLine,
) {
    let lo = s.x.max(0);
    let hi = (s.x + s.width as i32).min(SCREEN_WIDTH as i32);
    for screen_x in lo..hi {
        let (mut lx, mut ly) = mosaic_pos(ctx, s, screen_x, dy);
        if s.hflip {
            lx = s.width - 1 - lx;
        }
        if s.vflip {
            ly = s.height - 1 - ly;
        }
        let idx = s.texel(ctx, lx, ly);
        s.plot(ctx.mem, out, screen_x as usize, idx, line);
    }
}

/// Rotation/scaling parameter group `group`: PA, PB, PC, PD are the fourth halfword
/// of each of four consecutive OAM entries (GBATEK "LCD OBJ - OAM Rotation/Scaling
/// Parameters": "1st Group - PA=07000006, PB=0700000E, PC=07000016, PD=0700001E").
fn params(mem: &Mem, group: usize) -> [i32; 4] {
    let at = |i: usize| {
        let o = group * 32 + 6 + i * 8;
        i32::from(i16::from_le_bytes([mem.oam[o], mem.oam[o + 1]]))
    };
    [at(0), at(1), at(2), at(3)]
}

/// An affine sprite on row `dy` of its bounding box. Each box pixel (ix, iy) is
/// mapped around the box centre into the sprite's own image:
/// `u = (PA*(ix-cx) + PB*(iy-cy)) / 256 + width/2`, `v = (PC*(ix-cx) + PD*(iy-cy)) / 256 +
/// height/2`, and shows nothing where (u, v) falls outside the image: the box is
/// clipped to the displayed area (GBATEK "LCD OBJ - OAM Rotation/Scaling Parameters").
/// The double-size flag only grows the box, so a rotated sprite is not cut off.
fn draw_affine(ctx: &Ctx, s: &Sprite, dy: u32, out: &mut [u32; SCREEN_WIDTH], line: &mut ObjLine) {
    let [pa, pb, pc, pd] = params(ctx.mem, s.group);
    let (cx, cy) = ((s.box_w / 2) as i32, (s.box_h / 2) as i32);
    let (half_w, half_h) = ((s.width / 2) as i32, (s.height / 2) as i32);
    let lo = s.x.max(0);
    let hi = (s.x + s.box_w as i32).min(SCREEN_WIDTH as i32);
    for screen_x in lo..hi {
        let (ix, iy) = if s.mosaic && ctx.mosaic_on {
            let (lx, ly) = mosaic_pos(ctx, s, screen_x, dy);
            (lx as i32, ly as i32)
        } else {
            (screen_x - s.x, dy as i32)
        };
        let (rx, ry) = (ix - cx, iy - cy);
        let u = ((pa * rx + pb * ry) >> 8) + half_w;
        let v = ((pc * rx + pd * ry) >> 8) + half_h;
        if u < 0 || v < 0 || u >= s.width as i32 || v >= s.height as i32 {
            continue;
        }
        let idx = s.texel(ctx, u as u32, v as u32);
        s.plot(ctx.mem, out, screen_x as usize, idx, line);
    }
}
