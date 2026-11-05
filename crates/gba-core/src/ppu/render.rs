//! Line rendering: decode the registers, fill one line buffer per layer, then
//! composite them into the back buffer (DECISIONS #56).
//!
//! The layer buffers are what P2 (windows, blending, mosaic, affine) builds on:
//!
//! - `Layers::bg[n]` holds BGn's colour for the line, [`TRANSPARENT`] where the
//!   layer has no pixel. A BG renderer fills the whole line.
//! - `Layers::obj` holds one word per pixel (colour + OBJ priority + the
//!   semi-transparent and OBJ-window flags, see [`OBJ_EMPTY`]).
//! - `Layers::enable` is a per-pixel mask of layers allowed to show (bit n =
//!   layer n, bit 5 = colour effects). P1 fills it with "all"; P2 fills it from the
//!   windows.
//! - [`resolve`] returns the top **two** visible candidates per pixel (layer id +
//!   colour), P1 outputs the top one; blending needs the second as its target.

use super::{Ppu, bg, obj};
use crate::SCREEN_WIDTH;

/// One line of 15-bit colours.
pub(super) type Line = [u16; SCREEN_WIDTH];

/// Marks "no pixel" in a BG line and in the colour field of an OBJ word. Real colours
/// are 15 bits (bit 15 ignored), so bit 15 is free to carry this.
pub(super) const TRANSPARENT: u16 = 0x8000;
/// What forced blank shows (GBATEK "LCD I/O Display Control": white).
pub(super) const WHITE: u16 = 0x7FFF;

/// Layer ids used by [`resolve`] and the `enable` mask: BG0..=BG3 are 0..=3.
pub(super) const LAYER_OBJ: u8 = 4;
pub(super) const LAYER_BACKDROP: u8 = 5;
/// `Layers::enable` bit for the colour-effects flag (window "special effects").
const ENABLE_EFFECTS: u8 = 1 << 5;
/// BG0-3 and OBJ (bits 0-4) plus effects.
const ENABLE_ALL: u8 = 0x1F | ENABLE_EFFECTS;

/// An empty OBJ-line word: transparent, priority 0, no flags. Layout of an OBJ word:
/// bits 0-14 colour, bit 15 [`TRANSPARENT`], bits 16-17 priority (attr2 bits
/// 10-11), bit 18 [`OBJ_SEMI`], bit 19 [`OBJ_WINDOW`].
pub(super) const OBJ_EMPTY: u32 = TRANSPARENT as u32;
/// The sprite pixel came from a semi-transparent sprite (attr0 mode 1).
pub(super) const OBJ_SEMI: u32 = 1 << 18;
/// Some OBJ-window sprite (attr0 mode 2) covers this pixel. Such sprites draw no
/// colour; they only mark the OBJ window.
pub(super) const OBJ_WINDOW: u32 = 1 << 19;

/// Read-only views of the three memories a line is drawn from.
pub(super) struct Mem<'a> {
    pub vram: &'a [u8; 0x1_8000],
    pub palette: &'a [u8; 0x400],
    pub oam: &'a [u8; 0x400],
}

impl Mem<'_> {
    /// Palette entry `i` (0..256 BG, 256..512 OBJ) as a 15-bit colour.
    #[inline]
    pub fn colour(&self, i: usize) -> u16 {
        let i = (i & 0x1FF) * 2;
        u16::from_le_bytes([self.palette[i], self.palette[i + 1]]) & 0x7FFF
    }

    /// The 8 palette indices of one tile row at `addr` (4bpp: 4 bytes, low nibble
    /// is the left pixel; 8bpp: 8 bytes), mirrored if `hflip`.
    #[inline]
    pub fn tile_row(&self, addr: usize, is8: bool, hflip: bool) -> [u8; 8] {
        let mut t = [0u8; 8];
        if is8 {
            t.copy_from_slice(&self.vram[addr..addr + 8]);
        } else {
            for (pair, &b) in t.chunks_exact_mut(2).zip(&self.vram[addr..addr + 4]) {
                pair[0] = b & 0xF;
                pair[1] = b >> 4;
            }
        }
        if hflip {
            t.reverse();
        }
        t
    }
}

/// The registers a line needs, decoded once per line.
pub(super) struct Regs {
    pub dispcnt: u16,
    pub bgcnt: [u16; 4],
    pub hofs: [u16; 4],
    pub vofs: [u16; 4],
}

impl Regs {
    pub fn from_io(io: &[u16; 0x30]) -> Self {
        let mut r = Self {
            dispcnt: io[0],
            bgcnt: [
                io[0x008 >> 1],
                io[0x00A >> 1],
                io[0x00C >> 1],
                io[0x00E >> 1],
            ],
            hofs: [0; 4],
            vofs: [0; 4],
        };
        for bg in 0..4 {
            r.hofs[bg] = io[(0x010 >> 1) + bg * 2] & 0x1FF;
            r.vofs[bg] = io[(0x012 >> 1) + bg * 2] & 0x1FF;
        }
        r
    }

    /// BG mode (DISPCNT bits 0-2).
    #[inline]
    pub fn mode(&self) -> u16 {
        self.dispcnt & 7
    }

    #[inline]
    pub fn bg_enabled(&self, bg: usize) -> bool {
        self.dispcnt & (0x100 << bg) != 0
    }

    #[inline]
    pub fn bg_priority(&self, bg: usize) -> u16 {
        self.bgcnt[bg] & 3
    }
}

/// The per-line scratch: one buffer per layer.
pub(super) struct Layers {
    pub bg: [Line; 4],
    pub obj: [u32; SCREEN_WIDTH],
    pub enable: [u8; SCREEN_WIDTH],
}

impl Layers {
    pub fn new() -> Box<Self> {
        Box::new(Self {
            bg: [[TRANSPARENT; SCREEN_WIDTH]; 4],
            obj: [OBJ_EMPTY; SCREEN_WIDTH],
            enable: [ENABLE_ALL; SCREEN_WIDTH],
        })
    }
}

/// One visible pixel candidate at a position.
#[derive(Clone, Copy)]
pub(super) struct Candidate {
    /// Read by P2's blender (which layers are the first/second target).
    #[allow(dead_code)]
    pub layer: u8,
    pub colour: u16,
}

/// The two topmost visible pixels at `x`: `[top, second]`. A layer is visible if its
/// pixel is opaque and its bit is set in `enable[x]`. The backdrop always
/// qualifies, so when fewer than two layers show, the remaining slots are backdrop
/// (P2's blend must treat a backdrop top as "no second target").
///
/// `order` lists the active BGs sorted by (priority, index). An OBJ pixel goes in
/// front of every BG whose priority number is not lower than the OBJ's own, so
/// OBJ wins ties (GBATEK "LCD OBJ - Overview": "OBJ priority ... same as BG").
#[inline]
fn resolve(x: usize, layers: &Layers, regs: &Regs, order: &[u8], backdrop: u16) -> [Candidate; 2] {
    let none = Candidate {
        layer: LAYER_BACKDROP,
        colour: backdrop,
    };
    let mut out = [none; 2];
    let mut n = 0;
    let enable = layers.enable[x];
    let obj_word = layers.obj[x];
    let mut obj_pending = enable & (1 << LAYER_OBJ) != 0 && obj_word & u32::from(TRANSPARENT) == 0;
    let obj_priority = ((obj_word >> 16) & 3) as u16;
    let obj = Candidate {
        layer: LAYER_OBJ,
        colour: (obj_word & 0x7FFF) as u16,
    };
    for &bg in order {
        let bg = bg as usize;
        if obj_pending && obj_priority <= regs.bg_priority(bg) {
            out[n] = obj;
            n += 1;
            obj_pending = false;
            if n == 2 {
                return out;
            }
        }
        if enable >> bg & 1 != 0 {
            let colour = layers.bg[bg][x];
            if colour != TRANSPARENT {
                out[n] = Candidate {
                    layer: bg as u8,
                    colour,
                };
                n += 1;
                if n == 2 {
                    return out;
                }
            }
        }
    }
    if obj_pending {
        out[n] = obj;
    }
    out
}

impl Ppu {
    /// Draw line `y` (0..160) into the back buffer from the current state.
    pub(super) fn render_line(&mut self, y: u32) {
        let regs = Regs::from_io(&self.io);
        let start = y as usize * SCREEN_WIDTH;
        let row: &mut Line = (&mut self.back[start..start + SCREEN_WIDTH])
            .try_into()
            .expect("a line is SCREEN_WIDTH pixels");
        if regs.dispcnt & 0x80 != 0 {
            row.fill(WHITE);
            return;
        }
        let mem = Mem {
            vram: &self.vram,
            palette: &self.palette,
            oam: &self.oam,
        };
        let layers = &mut *self.layers;

        // Which BGs draw, and how (DISPCNT BG mode, GBATEK "LCD I/O Display Control").
        let mut active = [false; 4];
        match regs.mode() {
            0 => {
                for (bg, a) in active.iter_mut().enumerate() {
                    *a = regs.bg_enabled(bg);
                }
            }
            // P2: BG2 is affine in mode 1, BG2/BG3 in mode 2.
            1 => {
                for (bg, a) in active.iter_mut().enumerate().take(2) {
                    *a = regs.bg_enabled(bg);
                }
            }
            3..=5 => active[2] = regs.bg_enabled(2),
            _ => {}
        }
        for (bg, &a) in active.iter().enumerate() {
            if !a {
                continue;
            }
            let line = &mut layers.bg[bg];
            match regs.mode() {
                0 | 1 => bg::render_text(bg, &regs, &mem, y, line),
                3 => bg::render_mode3(&mem, y, line),
                4 => bg::render_mode4(&mem, regs.dispcnt & 0x10 != 0, y, line),
                _ => bg::render_mode5(&mem, regs.dispcnt & 0x10 != 0, y, line),
            }
        }

        // BGs front to back: by priority, then index (GBATEK "LCD I/O BG Control").
        let mut order = [0u8; 4];
        let mut n = 0;
        for priority in 0..4 {
            for (bg, &a) in active.iter().enumerate() {
                if a && regs.bg_priority(bg) == priority {
                    order[n] = bg as u8;
                    n += 1;
                }
            }
        }
        let order = &order[..n];

        let obj_drawn =
            regs.dispcnt & 0x1000 != 0 && obj::render_line(&regs, &mem, y, &mut layers.obj);

        // P2: fill `enable` from the windows here.
        layers.enable.fill(ENABLE_ALL);

        let backdrop = mem.colour(0);
        if order.is_empty() && !obj_drawn {
            row.fill(backdrop);
            return;
        }
        if !obj_drawn {
            // No sprite on this line: resolve against an empty OBJ line.
            layers.obj.fill(OBJ_EMPTY);
        }
        for (x, px) in row.iter_mut().enumerate() {
            *px = resolve(x, layers, &regs, order, backdrop)[0].colour;
        }
    }
}
