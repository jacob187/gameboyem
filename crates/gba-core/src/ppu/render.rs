//! Line rendering: decode the registers, fill one line buffer per layer, then
//! composite them into the back buffer.
//!
//! The layer buffers:
//!
//! - `Layers::bg[n]` holds BGn's colour for the line, [`TRANSPARENT`] where the
//!   layer has no pixel. A BG renderer fills the whole line.
//! - `Layers::obj` holds one word per pixel (colour + OBJ priority + the
//!   semi-transparent and OBJ-window flags, see [`OBJ_EMPTY`]).
//! - `Layers::enable` is a per-pixel mask of layers allowed to show (bit n =
//!   layer n, bit 5 = colour effects), filled from the windows (`window.rs`).
//! - [`resolve`] returns the top **two** visible candidates per pixel (layer id +
//!   colour); `effects.rs` blends them where BLDCNT (or a semi-transparent sprite)
//!   asks for it.

use super::{Ppu, bg, effects, obj, window};
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
pub(super) const ENABLE_EFFECTS: u8 = 1 << 5;
/// BG0-3 and OBJ (bits 0-4) plus effects.
pub(super) const ENABLE_ALL: u8 = 0x1F | ENABLE_EFFECTS;

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

/// One affine background's parameters (BG2 or BG3): the 8.8 fixed-point matrix and
/// the reference point latched for this line (20.8 fixed point, GBATEK "LCD I/O BG
/// Rotation/Scaling").
#[derive(Clone, Copy, Default)]
pub(super) struct Affine {
    pub pa: i32,
    pub pb: i32,
    pub pc: i32,
    pub pd: i32,
    pub x: i32,
    pub y: i32,
}

/// The registers a line needs, decoded once per line.
pub(super) struct Regs {
    pub dispcnt: u16,
    pub bgcnt: [u16; 4],
    pub hofs: [u16; 4],
    pub vofs: [u16; 4],
    /// BG2 and BG3 (index `bg - 2`).
    pub affine: [Affine; 2],
    pub mosaic: u16,
    pub win_h: [u16; 2],
    pub win_v: [u16; 2],
    pub winin: u16,
    pub winout: u16,
    pub bldcnt: u16,
    pub bldalpha: u16,
    pub bldy: u16,
}

impl Regs {
    /// `latch` holds the internal reference points `[bg2, bg3][x, y]`.
    pub fn from_io(io: &[u16; 0x30], latch: &[[i32; 2]; 2]) -> Self {
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
            affine: [Affine::default(); 2],
            mosaic: io[0x04C >> 1],
            win_h: [io[0x040 >> 1], io[0x042 >> 1]],
            win_v: [io[0x044 >> 1], io[0x046 >> 1]],
            winin: io[0x048 >> 1],
            winout: io[0x04A >> 1],
            bldcnt: io[0x050 >> 1],
            bldalpha: io[0x052 >> 1],
            bldy: io[0x054 >> 1],
        };
        for bg in 0..4 {
            r.hofs[bg] = io[(0x010 >> 1) + bg * 2] & 0x1FF;
            r.vofs[bg] = io[(0x012 >> 1) + bg * 2] & 0x1FF;
        }
        for (n, a) in r.affine.iter_mut().enumerate() {
            let base = (0x020 >> 1) + n * 8;
            // PA..PD are signed 16-bit.
            *a = Affine {
                pa: i32::from(io[base] as i16),
                pb: i32::from(io[base + 1] as i16),
                pc: i32::from(io[base + 2] as i16),
                pd: i32::from(io[base + 3] as i16),
                x: latch[n][0],
                y: latch[n][1],
            };
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

    /// BG mosaic `(horizontal, vertical)` sizes in pixels when BGxCNT bit 6 is set and
    /// the size is not 1x1 (MOSAIC bits 0-3 / 4-7 hold size - 1).
    pub fn bg_mosaic(&self, bg: usize) -> Option<(u32, u32)> {
        let h = u32::from(self.mosaic & 15) + 1;
        let v = u32::from((self.mosaic >> 4) & 15) + 1;
        (self.bgcnt[bg] & 0x40 != 0 && (h > 1 || v > 1)).then_some((h, v))
    }

    /// OBJ mosaic `(horizontal, vertical)` sizes (MOSAIC bits 8-11 / 12-15).
    pub fn obj_mosaic(&self) -> (u32, u32) {
        (
            u32::from((self.mosaic >> 8) & 15) + 1,
            u32::from((self.mosaic >> 12) & 15) + 1,
        )
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
    pub layer: u8,
    pub colour: u16,
}

/// The active BGs of a line, front to back, with their priorities precomputed so the
/// per-pixel loop does no register lookups.
pub(super) struct Order {
    bg: [u8; 4],
    priority: [u8; 4],
    n: usize,
}

impl Order {
    /// BGs by priority, then index (GBATEK "LCD I/O BG Control").
    fn new(regs: &Regs, active: &[bool; 4]) -> Self {
        let mut o = Self {
            bg: [0; 4],
            priority: [0; 4],
            n: 0,
        };
        for priority in 0..4u16 {
            for (bg, &a) in active.iter().enumerate() {
                if a && regs.bg_priority(bg) == priority {
                    o.bg[o.n] = bg as u8;
                    o.priority[o.n] = priority as u8;
                    o.n += 1;
                }
            }
        }
        o
    }
}

/// The topmost visible pixel at `x` and, if `SECOND`, the one below it: `[top,
/// second]`. A layer is visible if its pixel is opaque and its bit is set in
/// `enable[x]`. The backdrop always qualifies, so when fewer layers show, the
/// remaining slots are backdrop (a backdrop top has no second target, which the
/// blender checks).
///
/// An OBJ pixel goes in front of every BG whose priority number is not lower than the
/// OBJ's own, so OBJ wins ties (GBATEK "LCD OBJ - Overview": "OBJ priority ... same
/// as BG"). `SECOND = false` stops after the first candidate.
#[inline]
fn resolve<const SECOND: bool>(
    x: usize,
    layers: &Layers,
    order: &Order,
    backdrop: u16,
) -> [Candidate; 2] {
    let none = Candidate {
        layer: LAYER_BACKDROP,
        colour: backdrop,
    };
    let mut out = [none; 2];
    let mut n = 0;
    let last = usize::from(SECOND);
    let enable = layers.enable[x];
    let obj_word = layers.obj[x];
    let mut obj_pending = enable & (1 << LAYER_OBJ) != 0 && obj_word & u32::from(TRANSPARENT) == 0;
    let obj_priority = ((obj_word >> 16) & 3) as u8;
    let obj = Candidate {
        layer: LAYER_OBJ,
        colour: (obj_word & 0x7FFF) as u16,
    };
    for i in 0..order.n {
        if obj_pending && obj_priority <= order.priority[i] {
            out[n] = obj;
            if n == last {
                return out;
            }
            n += 1;
            obj_pending = false;
        }
        let bg = order.bg[i] as usize;
        if enable >> bg & 1 != 0 {
            let colour = layers.bg[bg][x];
            if colour != TRANSPARENT {
                out[n] = Candidate {
                    layer: bg as u8,
                    colour,
                };
                if n == last {
                    return out;
                }
                n += 1;
            }
        }
    }
    if obj_pending {
        out[n] = obj;
    }
    out
}

/// Whether colour effects can change nothing on a line: no effect is selected, or
/// none of the layers that can show (`visible`, a mask in the `enable`-mask layout:
/// the drawn BGs and the backdrop) is a first target. Only valid when the line has
/// no sprite pixel, so no semi-transparent sprite forces a blend.
#[inline]
fn effects_inert(regs: &Regs, visible: u8) -> bool {
    let mode = (regs.bldcnt >> 6) & 3;
    let first = (regs.bldcnt & 0x3F) as u8;
    mode == 0 || first & visible == 0
}

/// The fast composite: the line has no sprite pixel, no window and no colour
/// effect, so the pixel is simply the front-most opaque BG pixel or the backdrop.
/// Paint the BGs back to front over the backdrop; this is what [`resolve`] picks
/// (`order` is front to back, ties already broken by BG index).
fn composite_plain(row: &mut Line, layers: &Layers, order: &Order, backdrop: u16) {
    let Some(last) = order.n.checked_sub(1) else {
        row.fill(backdrop);
        return;
    };
    // The back-most layer lands on the backdrop in the same pass.
    let bottom = &layers.bg[usize::from(order.bg[last])];
    for (px, &c) in row.iter_mut().zip(bottom) {
        *px = if c == TRANSPARENT { backdrop } else { c };
    }
    for i in (0..last).rev() {
        let layer = &layers.bg[usize::from(order.bg[i])];
        for (px, &c) in row.iter_mut().zip(layer) {
            *px = if c == TRANSPARENT { *px } else { c };
        }
    }
}

impl Ppu {
    /// Draw line `y` (0..160) into the back buffer from the current state.
    pub(super) fn render_line(&mut self, y: u32) {
        self.render_line_with(y, true);
    }

    /// [`Ppu::render_line`], with the plain-composite fast path switchable (the
    /// tests compare it with the general path). Returns whether the fast path drew
    /// the line.
    pub(super) fn render_line_with(&mut self, y: u32, allow_fast: bool) -> bool {
        let regs = Regs::from_io(&self.io, &self.affine_latch);
        let start = y as usize * SCREEN_WIDTH;
        let row: &mut Line = (&mut self.back[start..start + SCREEN_WIDTH])
            .try_into()
            .expect("a line is SCREEN_WIDTH pixels");
        if regs.dispcnt & 0x80 != 0 {
            row.fill(WHITE);
            return false;
        }
        let mem = Mem {
            vram: &self.vram,
            palette: &self.palette,
            oam: &self.oam,
        };
        let layers = &mut *self.layers;

        // Which BGs draw, and how (DISPCNT BG mode, GBATEK "LCD I/O Display Control").
        let mut active = [false; 4];
        let bgs = match regs.mode() {
            0 => 0..4,
            1 => 0..3,
            2 => 2..4,
            3..=5 => 2..3,
            _ => 0..0,
        };
        for bg in bgs {
            active[bg] = regs.bg_enabled(bg);
        }
        for (bg, &a) in active.iter().enumerate() {
            if a {
                bg::render(bg, &regs, &mem, y, &mut layers.bg[bg]);
            }
        }
        let order = Order::new(&regs, &active);
        let backdrop = mem.colour(0);

        // `obj::render_line` clears the OBJ line itself, so it is filled exactly once.
        let obj_enabled = regs.dispcnt & 0x1000 != 0;
        let obj_line = if obj_enabled {
            obj::render_line(&regs, &mem, y, &mut layers.obj)
        } else {
            obj::ObjLine::default()
        };

        // No sprite pixel, no window, no colour effect: nothing but BG priority
        // and the backdrop decides the pixel. The OBJ and enable lines are not
        // read, so they are not filled either; every other path fills them first.
        let visible = active
            .iter()
            .enumerate()
            .fold(1 << LAYER_BACKDROP, |m, (bg, &a)| m | u8::from(a) << bg);
        if allow_fast
            && !obj_line.touched
            && regs.dispcnt & 0xE000 == 0
            && effects_inert(&regs, visible)
        {
            composite_plain(row, layers, &order, backdrop);
            return true;
        }
        if !obj_enabled {
            layers.obj.fill(OBJ_EMPTY);
        }

        window::fill_enable(&regs, y, layers, obj_line.touched);

        let blend = effects::Blend::new(&regs);
        if blend.active(obj_line.semi) {
            for (x, px) in row.iter_mut().enumerate() {
                let [top, second] = resolve::<true>(x, layers, &order, backdrop);
                *px = if layers.enable[x] & ENABLE_EFFECTS != 0 {
                    let semi = top.layer == LAYER_OBJ && layers.obj[x] & OBJ_SEMI != 0;
                    blend.apply(top, second, semi)
                } else {
                    top.colour
                };
            }
        } else {
            for (x, px) in row.iter_mut().enumerate() {
                *px = resolve::<false>(x, layers, &order, backdrop)[0].colour;
            }
        }
        false
    }
}

#[cfg(test)]
mod fast_path_tests {
    //! The plain composite and `render_text` against the general path and a
    //! straightforward pre-fill, per-pixel text renderer (`reference_text`), on
    //! seeded random registers and memory.

    use super::*;

    /// xorshift64*: deterministic, no dependencies.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn chance(&mut self, percent: u64) -> bool {
            self.below(100) < percent
        }
    }

    /// A PPU with random memory and registers. Windows are off 80% of the time,
    /// sprites absent or sparse, and BLDCNT is often inert, so many lines qualify
    /// for the fast path; the rest exercise the comparison against themselves.
    fn random_ppu(rng: &mut Rng) -> Ppu {
        let mut ppu = Ppu::new();
        let density = [2, 4, 8, 100][rng.below(4) as usize];
        for b in ppu.vram.iter_mut() {
            *b = if rng.below(100) < density {
                rng.next() as u8
            } else {
                0
            };
        }
        for b in ppu.palette.iter_mut() {
            *b = rng.next() as u8;
        }
        let mode = rng.below(6) as u16;
        let mut dispcnt = mode | (rng.below(16) as u16) << 8 | (rng.below(2) as u16) << 4;
        if rng.chance(50) {
            dispcnt |= 0x1000 | (rng.below(2) as u16) << 6;
        }
        if rng.chance(20) {
            dispcnt |= (rng.below(8) as u16) << 13;
        }
        ppu.write_io(0x000, dispcnt, 0xFFFF);
        for bg in 0..4u32 {
            ppu.write_io(0x008 + 2 * bg, rng.next() as u16 & 0xFFCF | 0x20, 0xFFFF);
            ppu.write_io(0x010 + 4 * bg, rng.next() as u16, 0xFFFF);
            ppu.write_io(0x012 + 4 * bg, rng.next() as u16, 0xFFFF);
        }
        for off in (0x020..0x040).step_by(2) {
            let v = if rng.chance(50) {
                [0x100, 0, 0, 0x100][((off - 0x20) / 2 % 4) as usize]
            } else {
                rng.next() as u16 & 0x03FF
            };
            ppu.write_io(off, v, 0xFFFF);
        }
        ppu.write_io(0x04C, rng.next() as u16, 0xFFFF);
        if rng.chance(80) {
            ppu.write_io(0x048, rng.next() as u16, 0xFFFF);
            ppu.write_io(0x04A, rng.next() as u16, 0xFFFF);
        }
        for off in [0x040, 0x042, 0x044, 0x046] {
            ppu.write_io(off, rng.next() as u16, 0xFFFF);
        }
        let bldcnt = match rng.below(5) {
            0 | 1 => 0,
            2 => rng.next() as u16 & 0x3F00 | (rng.below(4) as u16) << 6,
            _ => rng.next() as u16 & 0x3FFF,
        };
        ppu.write_io(0x050, bldcnt, 0xFFFF);
        ppu.write_io(0x052, rng.next() as u16, 0xFFFF);
        ppu.write_io(0x054, rng.next() as u16, 0xFFFF);
        if rng.chance(50) {
            // Every sprite hidden (shape 3 is not displayed).
            for o in (0..0x400).step_by(8) {
                ppu.oam[o..o + 2].copy_from_slice(&0xC000u16.to_le_bytes());
            }
        } else {
            for b in ppu.oam.iter_mut() {
                *b = rng.next() as u8;
            }
        }
        ppu
    }

    #[test]
    fn plain_composite_matches_the_general_path() {
        let (mut lines, mut fast) = (0, 0);
        for seed in 1..=400u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut ppu = random_ppu(&mut rng);
            for _ in 0..6 {
                let y = rng.below(160) as u32;
                let start = y as usize * SCREEN_WIDTH;
                let took = ppu.render_line_with(y, true);
                let quick: Vec<u16> = ppu.back[start..start + SCREEN_WIDTH].to_vec();
                assert!(!ppu.render_line_with(y, false));
                assert_eq!(
                    quick,
                    ppu.back[start..start + SCREEN_WIDTH],
                    "seed {seed} line {y} (fast path taken: {took})"
                );
                lines += 1;
                fast += usize::from(took);
            }
        }
        // The seeds must actually cover the fast path, or the test proves nothing.
        assert!(
            fast * 5 > lines,
            "{fast} of {lines} lines took the fast path"
        );
        assert!(fast < lines, "{fast} of {lines}");
    }

    /// The text renderer written the plain way (full pre-fill, per-pixel writes),
    /// kept as the oracle for the fast path.
    fn reference_text(bg: usize, regs: &Regs, mem: &Mem, y: u32, out: &mut Line) {
        let cnt = regs.bgcnt[bg];
        let char_base = usize::from((cnt >> 2) & 3) * 0x4000;
        let is8 = cnt & 0x80 != 0;
        let screen_base = usize::from((cnt >> 8) & 31) * 0x800;
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
        let mut x = -((px0 & 7) as i32);
        while x < SCREEN_WIDTH as i32 {
            let col = tx & if wide { 63 } else { 31 };
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
            if addr + row_bytes <= 0x1_0000 {
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

    #[test]
    fn text_renderer_matches_the_reference() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03));
            let ppu = random_ppu(&mut rng);
            let regs = Regs::from_io(&ppu.io, &ppu.affine_latch);
            let mem = Mem {
                vram: &ppu.vram,
                palette: &ppu.palette,
                oam: &ppu.oam,
            };
            for _ in 0..8 {
                let (bg, y) = (rng.below(4) as usize, rng.below(160) as u32);
                // Stale content in the output must not leak through either.
                let mut new = [rng.next() as u16; SCREEN_WIDTH];
                let mut old = [0; SCREEN_WIDTH];
                bg::render_text(bg, &regs, &mem, y, &mut new);
                reference_text(bg, &regs, &mem, y, &mut old);
                assert_eq!(new, old, "seed {seed} bg {bg} line {y}");
            }
        }
    }
}
