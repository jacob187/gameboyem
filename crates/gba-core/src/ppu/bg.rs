//! Background layers: text (tiled), affine (rotation/scaling) and bitmap backgrounds.
//! GBATEK "LCD VRAM Bitmap BG Modes", "LCD VRAM Character Data", "LCD VRAM BG Screen Data
//! Format", "LCD I/O BG Rotation/Scaling".

use super::render::{Affine, Line, Mem, Regs, TRANSPARENT};
use crate::SCREEN_WIDTH;

/// VRAM byte offset of bitmap frame 1 (modes 4 and 5, DISPCNT bit 4).
const FRAME1: usize = 0xA000;
/// BG tile data lives in the first 64 KiB of VRAM; the rest is OBJ tile memory.
const BG_TILE_LIMIT: usize = 0x1_0000;

/// Draw BG `bg` for line `y` into `out`, in whatever form the BG mode gives it: text
/// (modes 0, 1), affine map (mode 1 BG2, mode 2 BG2/BG3) or the affine bitmap (modes
/// 3-5 BG2). BG mosaic is applied here (GBATEK "LCD I/O Mosaic Function"): vertical
/// by drawing every line of a block from the block's first line, horizontal by
/// repeating the block's leftmost pixel.
pub(super) fn render(bg: usize, regs: &Regs, mem: &Mem, y: u32, out: &mut Line) {
    let (mh, mv) = regs.bg_mosaic(bg).unwrap_or((1, 1));
    // Lines to step back to the first line of the mosaic block.
    let back = y % mv;
    match (regs.mode(), bg) {
        (0, _) | (1, 0 | 1) => render_text(bg, regs, mem, y - back, out),
        (1 | 2, _) => render_affine_map(bg, regs, mem, back, out),
        _ => render_affine_bitmap(regs, mem, back, out),
    }
    if mh > 1 {
        for x in 0..SCREEN_WIDTH {
            out[x] = out[x - x % mh as usize];
        }
    }
}

/// The texel position of screen pixel 0 of an affine BG line, `back` lines above
/// the current latch (vertical mosaic holds the block's first line), then a step of
/// (PA, PC) per pixel.
struct AffineScan {
    u: i32,
    v: i32,
    pa: i32,
    pc: i32,
}

impl AffineScan {
    fn new(a: &Affine, back: u32) -> Self {
        let back = back as i32;
        Self {
            u: a.x - back * a.pb,
            v: a.y - back * a.pd,
            pa: a.pa,
            pc: a.pc,
        }
    }

    /// The integer texel (u, v) of the next pixel, then advance one pixel. Positions
    /// are 24.8 fixed point.
    #[inline]
    fn next(&mut self) -> (i32, i32) {
        let p = (self.u >> 8, self.v >> 8);
        self.u = self.u.wrapping_add(self.pa);
        self.v = self.v.wrapping_add(self.pc);
        p
    }
}

/// An affine (rotation/scaling) tile map BG: 8bpp, one-byte map entries, 16x16 to
/// 128x128 tiles, no flips. Outside the map the layer wraps if BGxCNT bit 13 is set
/// and is transparent otherwise (GBATEK "LCD I/O BG Control", "LCD VRAM BG Screen
/// Data Format").
fn render_affine_map(bg: usize, regs: &Regs, mem: &Mem, back: u32, out: &mut Line) {
    let cnt = regs.bgcnt[bg];
    let char_base = usize::from((cnt >> 2) & 3) * 0x4000;
    let screen_base = usize::from((cnt >> 8) & 31) * 0x800;
    let wrap = cnt & 0x2000 != 0;
    // 128, 256, 512 or 1024 pixels square.
    let size = 128i32 << (cnt >> 14);
    let tiles_wide = (size >> 3) as usize;
    let mut scan = AffineScan::new(&regs.affine[bg - 2], back);
    for px in out.iter_mut() {
        let (mut u, mut v) = scan.next();
        if wrap {
            u &= size - 1;
            v &= size - 1;
        } else if !(0..size).contains(&u) || !(0..size).contains(&v) {
            *px = TRANSPARENT;
            continue;
        }
        let tile =
            usize::from(mem.vram[screen_base + (v >> 3) as usize * tiles_wide + (u >> 3) as usize]);
        let addr = char_base + tile * 64 + (v & 7) as usize * 8 + (u & 7) as usize;
        // Tile data past 64 KiB is OBJ memory: the BG shows nothing there.
        let idx = if addr < BG_TILE_LIMIT {
            mem.vram[addr]
        } else {
            0
        };
        *px = if idx == 0 {
            TRANSPARENT
        } else {
            mem.colour(usize::from(idx))
        };
    }
}

/// BG2 in the bitmap modes, drawn through the same affine fetch as the tile maps.
/// Mode 3: 240x160 direct 15-bit colour. Mode 4: 240x160 8-bit palette indices (index
/// 0 transparent), two frames. Mode 5: 160x128 direct colour, two frames. Outside
/// the bitmap the layer is transparent: "The area overflow is ignored in Bitmap modes
/// (BG modes 3-5), the outside of the Bitmaps is always transparent" (GBATEK "LCD
/// I/O BG Rotation/Scaling"). With the BIOS-initialised identity matrix (PA = PD =
/// 0x100) this is the plain bitmap.
fn render_affine_bitmap(regs: &Regs, mem: &Mem, back: u32, out: &mut Line) {
    let mode = regs.mode();
    let (w, h) = if mode == 5 { (160, 128) } else { (240, 160) };
    let base = if mode != 3 && regs.dispcnt & 0x10 != 0 {
        FRAME1
    } else {
        0
    };
    let mut scan = AffineScan::new(&regs.affine[0], back);
    for px in out.iter_mut() {
        let (u, v) = scan.next();
        *px = if !(0..w).contains(&u) || !(0..h).contains(&v) {
            TRANSPARENT
        } else if mode == 4 {
            match mem.vram[base + (v * w + u) as usize] {
                0 => TRANSPARENT,
                idx => mem.colour(usize::from(idx)),
            }
        } else {
            let a = base + (v * w + u) as usize * 2;
            u16::from_le_bytes([mem.vram[a], mem.vram[a + 1]]) & 0x7FFF
        };
    }
}

/// A text-mode BG (BG0-3 in mode 0, BG0-1 in mode 1) for line `y`: 8x8 tiles from a
/// 32x32-entry screen block map, 1 or 2 (or 4) blocks big, scrolled by HOFS/VOFS.
pub(super) fn render_text(bg: usize, regs: &Regs, mem: &Mem, y: u32, out: &mut Line) {
    let cnt = regs.bgcnt[bg];
    let char_base = usize::from((cnt >> 2) & 3) * 0x4000;
    let is8 = cnt & 0x80 != 0;
    let screen_base = usize::from((cnt >> 8) & 31) * 0x800;
    // Size bits: 0 = 256x256, 1 = 512x256, 2 = 256x512, 3 = 512x512 (pixels).
    let size = cnt >> 14;
    let wide = size & 1 != 0;
    let tall = size & 2 != 0;
    let width_mask = if wide { 511 } else { 255 };
    let height_mask = if tall { 511 } else { 255 };
    let tile_bytes = if is8 { 64 } else { 32 };
    let row_bytes = if is8 { 8 } else { 4 };

    let py = (y + u32::from(regs.vofs[bg])) & height_mask;
    let ty = (py >> 3) as usize;
    let fine_y = (py & 7) as usize;
    let px0 = u32::from(regs.hofs[bg]) & width_mask;
    let mut tx = (px0 >> 3) as usize;
    // Screen x of the left edge of the first (partially visible) tile. The tiles
    // cover the line exactly once, so every column is written here, with
    // TRANSPARENT where a tile has no pixel: no pre-fill of the line.
    let mut x = -((px0 & 7) as i32);
    while x < SCREEN_WIDTH as i32 {
        let col = tx & if wide { 63 } else { 31 };
        // Screen blocks are 32x32 entries; bigger maps lay blocks out left to right,
        // then top to bottom.
        let block = match size {
            0 => 0,
            1 => col >> 5,
            2 => ty >> 5,
            _ => (col >> 5) + 2 * (ty >> 5),
        };
        let entry_addr = screen_base + block * 0x800 + ((ty & 31) * 32 + (col & 31)) * 2;
        let entry = u16::from_le_bytes([mem.vram[entry_addr], mem.vram[entry_addr + 1]]);
        let tile = usize::from(entry & 0x3FF);
        let (hflip, vflip) = (entry & 0x400 != 0, entry & 0x800 != 0);
        let bank = usize::from(entry >> 12) * 16;
        let row = if vflip { 7 - fine_y } else { fine_y };
        let addr = char_base + tile * tile_bytes + row * row_bytes;
        let lo = (-x).max(0) as usize;
        let hi = (SCREEN_WIDTH as i32 - x).min(8) as usize;
        let dst = &mut out[(x + lo as i32) as usize..(x + hi as i32) as usize];
        // Tile data past 64 KiB is OBJ memory: the BG shows nothing there. A tile
        // row of all zero texels shows nothing either, so skip decoding it.
        if addr + row_bytes > BG_TILE_LIMIT || row_is_empty(&mem.vram[addr..addr + row_bytes]) {
            dst.fill(TRANSPARENT);
        } else {
            let texels = mem.tile_row(addr, is8, hflip);
            for (px, &idx) in dst.iter_mut().zip(&texels[lo..hi]) {
                *px = if idx == 0 {
                    TRANSPARENT
                } else if is8 {
                    mem.colour(usize::from(idx))
                } else {
                    mem.colour(bank + usize::from(idx))
                };
            }
        }
        x += 8;
        tx += 1;
    }
}

/// Whether a tile row (4 or 8 bytes) has no non-zero texel.
#[inline]
fn row_is_empty(row: &[u8]) -> bool {
    match <[u8; 4]>::try_from(row) {
        Ok(four) => u32::from_le_bytes(four) == 0,
        Err(_) => row.iter().all(|&b| b == 0),
    }
}
