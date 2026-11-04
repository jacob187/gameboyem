//! CGB colour scanline renderer (Pan Docs: "VRAM Tile Maps" attributes, "OAM",
//! "LCD Color Palettes (CGB only)", "Object Priority and Conflicts").
//!
//! A separate path from `render_line` so the DMG renderer carries no per-pixel
//! model branch. Same structure: the whole line is drawn at the start of mode 3,
//! BG / window first (colour id plus attribute per pixel), sprites over it.
//! Palette RAM is read once per line, so a mid-line palette write applies from
//! the next line, like the other mid-line register changes.

use super::render::{ATTR_BG_PRIORITY, ATTR_X_FLIP, ATTR_Y_FLIP, MAX_SPRITES_PER_LINE, color_id};
use super::{
    CGB_WHITE, LCDC_BG_ENABLE, LCDC_BG_MAP, LCDC_OBJ_ENABLE, LCDC_TILE_DATA, LCDC_WIN_MAP,
    OPRI_X_PRIORITY, PALETTE_RAM_SIZE, Ppu, VRAM_BANK_SIZE,
};
use crate::SCREEN_WIDTH;

/// BG map / OAM attribute bits shared by both (Pan Docs).
const ATTR_BANK: u8 = 0x08;
const ATTR_PALETTE_MASK: u8 = 0x07;

/// Eight palettes of four RGB555 colours.
type Palettes = [[u16; 4]; 8];

/// Decode a palette RAM (little-endian words; bit 15 is not stored).
fn decode(ram: &[u8; PALETTE_RAM_SIZE]) -> Palettes {
    let mut out = [[CGB_WHITE; 4]; 8];
    for (i, c) in ram.chunks_exact(2).enumerate() {
        out[i / 4][i % 4] = u16::from_le_bytes([c[0], c[1]]) & CGB_WHITE;
    }
    out
}

/// One sprite's decoded row for the current line.
#[derive(Clone, Copy)]
struct SpriteRow {
    x: i16,
    lo: u8,
    hi: u8,
    flags: u8,
    /// OAM index (for the CGB drawing order).
    index: u8,
}

impl Ppu {
    /// Low/high bitplane bytes of one tile row in VRAM `bank`.
    fn tile_row_in_bank(&self, bank: u8, unsigned: bool, tile: u8, row: u8) -> (u8, u8) {
        let base = if unsigned {
            usize::from(tile) * 16
        } else {
            (0x1000 + i32::from(tile as i8) * 16) as usize
        };
        let a = usize::from(bank) * VRAM_BANK_SIZE + base + usize::from(row) * 2;
        (self.vram[a], self.vram[a + 1])
    }

    /// Draw the current line into the RGB555 back buffer.
    pub(super) fn render_line_cgb(&mut self) {
        let ly = self.ly;
        let mut ids = [0u8; SCREEN_WIDTH];
        let mut attrs = [0u8; SCREEN_WIDTH];
        // LCDC.0 clear does not blank the BG on a CGB; it only removes its
        // priority over sprites (see `render_sprites_cgb`).
        self.render_bg_cgb(ly, &mut ids, &mut attrs);
        let bg = decode(&self.bg_pal);
        let mut line = [0u16; SCREEN_WIDTH];
        for ((out, &id), &attr) in line.iter_mut().zip(&ids).zip(&attrs) {
            *out = bg[usize::from(attr & ATTR_PALETTE_MASK)][usize::from(id)];
        }
        if self.lcdc & LCDC_OBJ_ENABLE != 0 {
            self.render_sprites_cgb(ly, &ids, &attrs, &mut line);
        }
        let start = usize::from(ly) * SCREEN_WIDTH;
        if let Some(fb) = self.cgb_back.as_deref_mut() {
            fb[start..start + SCREEN_WIDTH].copy_from_slice(&line);
        }
    }

    /// BG + window colour ids and attributes (palette in bits 0-2, BG-to-OAM
    /// priority in bit 7) for one line. The attribute byte comes from the bank 1
    /// tile map at the same point the tile number is fetched.
    fn render_bg_cgb(
        &mut self,
        ly: u8,
        ids: &mut [u8; SCREEN_WIDTH],
        attrs: &mut [u8; SCREEN_WIDTH],
    ) {
        let unsigned = self.lcdc & LCDC_TILE_DATA != 0;
        let bg_map = if self.lcdc & LCDC_BG_MAP != 0 {
            0x1C00
        } else {
            0x1800
        };
        let win_map = if self.lcdc & LCDC_WIN_MAP != 0 {
            0x1C00
        } else {
            0x1800
        };
        let window = self.window_on_line();
        let win_left = i16::from(self.wx) - 7;
        let win_y = self.win_line;
        let bg_y = ly.wrapping_add(self.scy);

        let (mut lo, mut hi, mut attr) = (0, 0, 0);
        let mut in_window = false;
        for (x, (id, out_attr)) in ids.iter_mut().zip(attrs.iter_mut()).enumerate() {
            let win_here = window && x as i16 >= win_left;
            let (map, px, py) = if win_here {
                (win_map, (x as i16 - win_left) as u8, win_y)
            } else {
                (bg_map, (x as u8).wrapping_add(self.scx), bg_y)
            };
            if x == 0 || px % 8 == 0 || win_here != in_window {
                in_window = win_here;
                let at = map + usize::from(py / 8) * 32 + usize::from(px / 8);
                let tile = self.vram[at];
                attr = self.vram[VRAM_BANK_SIZE + at];
                let row = if attr & ATTR_Y_FLIP != 0 {
                    7 - py % 8
                } else {
                    py % 8
                };
                let bank = u8::from(attr & ATTR_BANK != 0);
                (lo, hi) = self.tile_row_in_bank(bank, unsigned, tile, row);
            }
            let bit = if attr & ATTR_X_FLIP != 0 {
                px % 8
            } else {
                7 - px % 8
            };
            *id = color_id(lo, hi, bit);
            *out_attr = attr & (ATTR_PALETTE_MASK | ATTR_BG_PRIORITY);
        }
        if window {
            self.win_line = self.win_line.wrapping_add(1);
        }
    }

    /// Composite the selected sprites over `line`.
    ///
    /// The first sprite with an opaque pixel at x owns it (the others are not
    /// consulted, even when it loses to the BG). Then the BG wins when LCDC.0 is
    /// set, its colour is 1-3 and either its attribute bit 7 or the sprite's is
    /// set. With LCDC.0 clear sprites are always on top.
    fn render_sprites_cgb(
        &self,
        ly: u8,
        ids: &[u8; SCREEN_WIDTH],
        attrs: &[u8; SCREEN_WIDTH],
        line: &mut [u16; SCREEN_WIDTH],
    ) {
        let height = i16::from(self.sprite_height());
        let master = self.lcdc & LCDC_BG_ENABLE != 0;
        let obj = decode(&self.obj_pal);
        let mut rows = [SpriteRow {
            x: 0,
            lo: 0,
            hi: 0,
            flags: 0,
            index: 0,
        }; MAX_SPRITES_PER_LINE];
        let count = usize::from(self.line_sprite_count);
        for (row, &i) in rows.iter_mut().zip(&self.line_sprites[..count]) {
            let o = usize::from(i) * 4;
            let y = i16::from(self.oam[o]) - 16;
            let mut tile = self.oam[o + 2];
            let flags = self.oam[o + 3];
            let mut r = i16::from(ly) - y;
            if flags & ATTR_Y_FLIP != 0 {
                r = height - 1 - r;
            }
            if height == 16 {
                tile &= 0xFE;
            }
            let bank = u8::from(flags & ATTR_BANK != 0);
            let (lo, hi) = self.tile_row_in_bank(bank, true, tile, r as u8);
            *row = SpriteRow {
                x: i16::from(self.oam[o + 1]) - 8,
                lo,
                hi,
                flags,
                index: i,
            };
        }
        // `line_sprites` is in X order (the fetch order mode 3's length needs);
        // OPRI bit 0 clear draws by OAM index instead (DMG style is X order).
        if self.opri & OPRI_X_PRIORITY == 0 {
            rows[..count].sort_unstable_by_key(|r| r.index);
        }

        for (x, out) in line.iter_mut().enumerate() {
            for row in &rows[..count] {
                let col = x as i16 - row.x;
                if !(0..8).contains(&col) {
                    continue;
                }
                let bit = if row.flags & ATTR_X_FLIP != 0 {
                    col
                } else {
                    7 - col
                };
                let id = color_id(row.lo, row.hi, bit as u8);
                if id == 0 {
                    continue;
                }
                let bg_wins = master
                    && ids[x] != 0
                    && (attrs[x] & ATTR_BG_PRIORITY != 0 || row.flags & ATTR_BG_PRIORITY != 0);
                if !bg_wins {
                    *out = obj[usize::from(row.flags & ATTR_PALETTE_MASK)][usize::from(id)];
                }
                break;
            }
        }
    }
}
