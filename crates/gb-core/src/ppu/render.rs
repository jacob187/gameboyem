//! Scanline renderer, OAM scan and mode 3 length (Pan Docs: "Tile Data",
//! "Tile Maps", "Scrolling", "Window", "OAM", "Object Priority and Conflicts",
//! "Mode 3 length").

use super::{
    LCDC_BG_ENABLE, LCDC_BG_MAP, LCDC_OBJ_ENABLE, LCDC_OBJ_TALL, LCDC_TILE_DATA, LCDC_WIN_ENABLE,
    LCDC_WIN_MAP, MODE3_BASE_DOTS, Ppu,
};
use crate::SCREEN_WIDTH;

/// OAM sprite attribute bits (Pan Docs: "OAM" attributes/flags).
const ATTR_BG_PRIORITY: u8 = 0x80;
const ATTR_Y_FLIP: u8 = 0x40;
const ATTR_X_FLIP: u8 = 0x20;
const ATTR_PALETTE: u8 = 0x10;

const MAX_SPRITES_PER_LINE: usize = 10;
/// OAM X values from here on place a sprite fully off the right edge.
const SPRITE_X_OFFSCREEN: u16 = 168;
/// See [`Ppu::compute_mode3_len`].
const SPRITE_PENALTY_PHASE: u16 = 3;

/// One sprite's decoded row for the current line.
#[derive(Clone, Copy)]
struct SpriteRow {
    /// Screen X of the leftmost pixel (may be negative / past the right edge).
    x: i16,
    lo: u8,
    hi: u8,
    flags: u8,
}

impl Ppu {
    fn sprite_height(&self) -> u8 {
        if self.lcdc & LCDC_OBJ_TALL != 0 {
            16
        } else {
            8
        }
    }

    /// OAM scan: pick the first 10 sprites (in OAM order) overlapping this
    /// line, then order them by drawing priority. Sprites with X = 0 or
    /// X >= 168 are invisible but still occupy a slot (Pan Docs: "OAM Scan").
    pub(super) fn scan_oam(&mut self) {
        let height = i16::from(self.sprite_height());
        let ly = i16::from(self.ly);
        let mut count = 0usize;
        for i in 0..40u8 {
            let y = i16::from(self.oam[usize::from(i) * 4]) - 16;
            if ly >= y && ly < y + height {
                self.line_sprites[count] = i;
                count += 1;
                if count == MAX_SPRITES_PER_LINE {
                    break;
                }
            }
        }
        // DMG priority: lower X wins, ties go to the lower OAM index. The
        // stable sort keeps OAM order within equal X.
        let oam = &self.oam;
        self.line_sprites[..count].sort_by_key(|&i| oam[usize::from(i) * 4 + 1]);
        self.line_sprite_count = count as u8;
    }

    /// True if the window is drawn on the current line (Pan Docs: "Window").
    pub(super) fn window_on_line(&self) -> bool {
        self.lcdc & LCDC_BG_ENABLE != 0
            && self.lcdc & LCDC_WIN_ENABLE != 0
            && self.wy_triggered
            && self.wx <= 166
    }

    /// Mode 3 length in dots: 172 base, plus `SCX % 8` fine-scroll discard,
    /// plus 6 for the window start, plus per-sprite fetch penalties
    /// (Pan Docs: "Mode 3 length"). The sprite penalty is the commonly used
    /// approximation: 6 dots per sprite, plus up to 5 more for the first sprite
    /// that lands in a given BG tile depending on its alignment within it.
    /// Sprites with X >= 168 are never fetched and cost nothing, and the sum is
    /// 3 dots short of that model: the table in mooneye
    /// `intr_2_mode0_timing_sprites` (ceil of the penalty in M-cycles, measured
    /// against the 4-dot mode 0 edge) fits `floor(penalty / 4)` for every
    /// sprite layout it covers.
    pub(super) fn compute_mode3_len(&self) -> u16 {
        let mut len = MODE3_BASE_DOTS + u16::from(self.scx % 8);
        if self.window_on_line() {
            len += 6;
        }
        if self.lcdc & LCDC_OBJ_ENABLE != 0 {
            let mut seen_tiles = [u16::MAX; MAX_SPRITES_PER_LINE];
            let mut penalty = 0;
            for (n, &i) in self.line_sprites[..usize::from(self.line_sprite_count)]
                .iter()
                .enumerate()
            {
                let oam_x = u16::from(self.oam[usize::from(i) * 4 + 1]);
                if oam_x >= SPRITE_X_OFFSCREEN {
                    continue;
                }
                let x = oam_x + u16::from(self.scx);
                let tile = x / 8;
                penalty += 6;
                if !seen_tiles[..n].contains(&tile) {
                    penalty += 5u16.saturating_sub(x % 8);
                }
                seen_tiles[n] = tile;
            }
            len += penalty.saturating_sub(SPRITE_PENALTY_PHASE);
        }
        len
    }

    /// Low/high bitplane bytes of one tile row. `unsigned` selects the
    /// 0x8000 addressing mode; otherwise tile indices are signed from 0x9000.
    fn bg_tile_row(&self, unsigned: bool, tile: u8, row: u8) -> (u8, u8) {
        let base = if unsigned {
            usize::from(tile) * 16
        } else {
            (0x1000 + i32::from(tile as i8) * 16) as usize
        };
        let a = base + usize::from(row) * 2;
        (self.vram[a], self.vram[a + 1])
    }

    /// Draw the current line into the back buffer.
    pub(super) fn render_line(&mut self) {
        let ly = self.ly;
        // With LCDC.0 clear the BG and window are colour 0, which BGP still
        // maps (SameBoy fills the line with BGP[1:0]); Pan Docs' "blank
        // (white)" is shorthand for that.
        let mut bg_ids = [0u8; SCREEN_WIDTH];
        if self.lcdc & LCDC_BG_ENABLE != 0 {
            self.render_bg(ly, &mut bg_ids);
        }
        let mut line = [0u8; SCREEN_WIDTH];
        for (out, &id) in line.iter_mut().zip(&bg_ids) {
            *out = (self.bgp >> (id * 2)) & 3;
        }
        if self.lcdc & LCDC_OBJ_ENABLE != 0 {
            self.render_sprites(ly, &bg_ids, &mut line);
        }
        let start = usize::from(ly) * SCREEN_WIDTH;
        self.back[start..start + SCREEN_WIDTH].copy_from_slice(&line);
    }

    /// Background + window colour indices (pre-palette) for one line.
    fn render_bg(&mut self, ly: u8, ids: &mut [u8; SCREEN_WIDTH]) {
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
        // WX is the window's left edge + 7; WX < 7 clips its first columns.
        let win_left = i16::from(self.wx) - 7;
        let win_y = self.win_line;
        let bg_y = ly.wrapping_add(self.scy);

        for (x, id) in ids.iter_mut().enumerate() {
            let (map, px, py) = if window && x as i16 >= win_left {
                (win_map, (x as i16 - win_left) as u8, win_y)
            } else {
                (bg_map, (x as u8).wrapping_add(self.scx), bg_y)
            };
            let tile = self.vram[map + usize::from(py / 8) * 32 + usize::from(px / 8)];
            let (lo, hi) = self.bg_tile_row(unsigned, tile, py % 8);
            let bit = 7 - (px % 8);
            *id = ((lo >> bit) & 1) | (((hi >> bit) & 1) << 1);
        }
        if window {
            self.win_line = self.win_line.wrapping_add(1);
        }
    }

    /// Composite the selected sprites over `line`.
    fn render_sprites(&self, ly: u8, bg_ids: &[u8; SCREEN_WIDTH], line: &mut [u8; SCREEN_WIDTH]) {
        let height = i16::from(self.sprite_height());
        let mut rows = [SpriteRow {
            x: 0,
            lo: 0,
            hi: 0,
            flags: 0,
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
            let a = usize::from(tile) * 16 + (r as usize) * 2;
            *row = SpriteRow {
                x: i16::from(self.oam[o + 1]) - 8,
                lo: self.vram[a],
                hi: self.vram[a + 1],
                flags,
            };
        }

        for (x, out) in line.iter_mut().enumerate() {
            // First (highest-priority) sprite with an opaque pixel here wins.
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
                let id = ((row.lo >> bit) & 1) | (((row.hi >> bit) & 1) << 1);
                if id == 0 {
                    continue;
                }
                // BG-over-OBJ only hides the sprite behind BG colours 1-3.
                if row.flags & ATTR_BG_PRIORITY == 0 || bg_ids[x] == 0 {
                    let pal = if row.flags & ATTR_PALETTE != 0 {
                        self.obp1
                    } else {
                        self.obp0
                    };
                    *out = (pal >> (id * 2)) & 3;
                }
                break;
            }
        }
    }
}
