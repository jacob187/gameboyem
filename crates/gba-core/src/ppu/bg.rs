//! Background layers: bitmap modes 3-5 and text (tiled) backgrounds.
//! GBATEK "LCD VRAM Bitmap BG Modes", "LCD VRAM Character Data", "LCD VRAM BG Screen Data Format".

use super::render::{Line, Mem, Regs, TRANSPARENT};
use crate::SCREEN_WIDTH;

/// VRAM byte offset of bitmap frame 1 (modes 4 and 5, DISPCNT bit 4).
const FRAME1: usize = 0xA000;
/// BG tile data lives in the first 64 KiB of VRAM; the rest is OBJ tile memory.
const BG_TILE_LIMIT: usize = 0x1_0000;

/// Mode 3: 240x160 direct 15-bit colour.
pub(super) fn render_mode3(mem: &Mem, y: u32, out: &mut Line) {
    let base = y as usize * SCREEN_WIDTH * 2;
    for (px, c) in out
        .iter_mut()
        .zip(mem.vram[base..base + SCREEN_WIDTH * 2].chunks_exact(2))
    {
        *px = u16::from_le_bytes([c[0], c[1]]) & 0x7FFF;
    }
}

/// Mode 4: 240x160 8-bit palette indices, two frames. Index 0 is transparent
/// (the backdrop shows through).
pub(super) fn render_mode4(mem: &Mem, frame1: bool, y: u32, out: &mut Line) {
    let base = if frame1 { FRAME1 } else { 0 } + y as usize * SCREEN_WIDTH;
    for (px, &idx) in out.iter_mut().zip(&mem.vram[base..base + SCREEN_WIDTH]) {
        *px = if idx == 0 {
            TRANSPARENT
        } else {
            mem.colour(idx as usize)
        };
    }
}

/// Mode 5: 160x128 direct 15-bit colour, two frames. Outside the 160x128 area the
/// layer is transparent (the backdrop shows).
pub(super) fn render_mode5(mem: &Mem, frame1: bool, y: u32, out: &mut Line) {
    const W: usize = 160;
    const H: u32 = 128;
    out.fill(TRANSPARENT);
    if y >= H {
        return;
    }
    let base = if frame1 { FRAME1 } else { 0 } + y as usize * W * 2;
    for (px, c) in out[..W]
        .iter_mut()
        .zip(mem.vram[base..base + W * 2].chunks_exact(2))
    {
        *px = u16::from_le_bytes([c[0], c[1]]) & 0x7FFF;
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

    out.fill(TRANSPARENT);
    let py = (y + u32::from(regs.vofs[bg])) & height_mask;
    let ty = (py >> 3) as usize;
    let fine_y = (py & 7) as usize;
    let px0 = u32::from(regs.hofs[bg]) & width_mask;
    let mut tx = (px0 >> 3) as usize;
    // Screen x of the left edge of the first (partially visible) tile.
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
        // Tile data past 64 KiB is OBJ memory: the BG shows nothing there.
        if addr + row_bytes <= BG_TILE_LIMIT {
            let texels = mem.tile_row(addr, is8, hflip);
            let lo = (-x).max(0) as usize;
            let hi = (SCREEN_WIDTH as i32 - x).min(8) as usize;
            for (i, &idx) in texels.iter().enumerate().take(hi).skip(lo) {
                if idx != 0 {
                    let pal = if is8 {
                        usize::from(idx)
                    } else {
                        bank + usize::from(idx)
                    };
                    out[(x + i as i32) as usize] = mem.colour(pal);
                }
            }
        }
        x += 8;
        tx += 1;
    }
}
