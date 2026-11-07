//! GRAINS: a falling-sand toy in raw mode 4 (240x160, 8-bit paletted, two
//! pages). agb has no public bitmap-mode API, so this talks to DISPCNT, the
//! palette and VRAM directly; agb is only used for input and VBlank waits.
//! D-pad moves the brush, A pours, B erases, L/R pick the material, SELECT
//! toggles the faucet, START clears. Everything is deterministic from input.
//!
//! The loop must finish inside one frame or the page flip lands mid-scan and
//! the game slows down, so a frame redraws only what changed on the page it is
//! about to show: [`Shown`] remembers, per page, the material drawn in each
//! cell, the cursor box and the HUD characters (a page is two frames stale).
//! The two per-cell loops, `Sim::step` and `Sim::render`, are placed in IWRAM
//! (thumb code from the 32-bit zero-wait bus; from ROM they cost about twice as
//! much), which `#[inline(never)]` keeps from being folded back into ROM code.
#![no_std]
#![no_main]

use agb::input::{Button, ButtonController};
use agb::interrupt::VBlank;
use core::ptr::{read_volatile, write_volatile};
use gba_homebrew::font;

const DISPCNT: *mut u16 = 0x0400_0000 as *mut u16;
const PALETTE: usize = 0x0500_0000;
const VRAM: usize = 0x0600_0000;
const PAGE_SIZE: usize = 0xA000;
const W: usize = 240;
const H: usize = 160;
/// Mode 4 + BG2 on; bit 4 picks the displayed page.
const MODE4_BG2: u16 = 0x0404;

const CELL: usize = 4;
const GW: usize = W / CELL;
const GH: usize = 36;
const HUD_Y: usize = GH * CELL;

// Cell contents; MOVED marks a cell already updated this frame.
const EMPTY: u8 = 0;
const SAND: u8 = 1;
const WATER: u8 = 2;
const STONE: u8 = 3;
const MOVED: u8 = 0x80;
const MATERIALS: [u8; 3] = [SAND, WATER, STONE];
const NAMES: [&[u8]; 3] = [b"SAND", b"WATER", b"STONE"];

// Palette indices.
const C_BG: u8 = 0;
const C_HUD: u8 = 1;
const C_TEXT: u8 = 2;
const C_CURSOR_A: u8 = 3;
const C_CURSOR_B: u8 = 4;
const C_BLINK: u8 = 5;
const C_STONE: u8 = 8; // 8..=9
const C_SAND: u8 = 16; // 16..=19
const C_WATER: u8 = 24; // 24..=31, rotated every frame in play
const C_RAINBOW: u8 = 40; // 40..=47, rotated on the title
/// "Not drawn yet" in a [`Shown`]: no material or ASCII byte is 0xFF.
const UNKNOWN: u8 = 0xFF;

const TITLE_TIMEOUT: u32 = 600;
const FAUCET_CAP: usize = 1200;

const fn rgb(r: u16, g: u16, b: u16) -> u16 {
    r | (g << 5) | (b << 10)
}

fn set_palette(i: u8, c: u16) {
    // SAFETY: palette RAM is always mapped; halfword write.
    unsafe { write_volatile((PALETTE + 2 * i as usize) as *mut u16, c) }
}

fn page_addr(page: usize) -> usize {
    VRAM + page * PAGE_SIZE
}

/// Plot one pixel with a read-modify-write of the halfword (VRAM ignores
/// byte writes).
fn put_px(page: usize, x: usize, y: usize, idx: u8) {
    if x >= W || y >= H {
        return;
    }
    let addr = (page_addr(page) + y * W + (x & !1)) as *mut u16;
    // SAFETY: inside the 240x160 page, halfword aligned.
    unsafe {
        let v = read_volatile(addr);
        let nv = if x & 1 == 0 {
            (v & 0xFF00) | idx as u16
        } else {
            (v & 0x00FF) | ((idx as u16) << 8)
        };
        write_volatile(addr, nv);
    }
}

fn fill_rect(page: usize, x: usize, y: usize, w: usize, h: usize, idx: u8) {
    for yy in y..y + h {
        for xx in x..x + w {
            put_px(page, xx, yy, idx);
        }
    }
}

/// Word-wide fill of whole rows `y0..y1` (x is always 0..240).
fn fill_rows(page: usize, y0: usize, y1: usize, idx: u8) {
    let word = u32::from(idx) * 0x0101_0101;
    let base = (page_addr(page) + y0 * W) as *mut u32;
    for i in 0..(y1 - y0) * W / 4 {
        // SAFETY: inside the page, word aligned.
        unsafe { write_volatile(base.add(i), word) }
    }
}

fn glyph(page: usize, x: usize, y: usize, scale: usize, c: u8, idx: u8) {
    for (ry, row) in font::rows(c).into_iter().enumerate() {
        for rx in 0..font::WIDTH {
            if row & (0b100 >> rx) != 0 {
                fill_rect(page, x + rx * scale, y + ry * scale, scale, scale, idx);
            }
        }
    }
}

/// Where character `n` of a string starting at `x` goes.
fn glyph_x(x: usize, n: usize, scale: usize) -> usize {
    x + n * (font::WIDTH + 1) * scale
}

fn text(page: usize, x: usize, y: usize, scale: usize, s: &[u8], idx: u8) {
    for (n, &c) in s.iter().enumerate() {
        glyph(page, glyph_x(x, n, scale), y, scale, c, idx);
    }
}

fn set_page(page: usize) {
    // SAFETY: DISPCNT is always mapped.
    unsafe { write_volatile(DISPCNT, MODE4_BG2 | ((page as u16) << 4)) }
}

fn init_palette() {
    set_palette(C_BG, rgb(2, 2, 6));
    set_palette(C_HUD, rgb(5, 5, 11));
    set_palette(C_TEXT, rgb(28, 28, 31));
    set_palette(C_CURSOR_A, rgb(31, 31, 31));
    set_palette(C_CURSOR_B, rgb(31, 26, 4));
    set_palette(C_BLINK, rgb(31, 31, 31));
    set_palette(C_STONE, rgb(14, 14, 16));
    set_palette(C_STONE + 1, rgb(11, 11, 13));
    for (i, c) in [
        rgb(26, 21, 9),
        rgb(24, 19, 8),
        rgb(28, 23, 11),
        rgb(25, 20, 7),
    ]
    .into_iter()
    .enumerate()
    {
        set_palette(C_SAND + i as u8, c);
    }
}

/// Rotate the water blues by `t` steps (play; the title draws no water).
fn animate_water(t: u32) {
    for i in 0..8u32 {
        let k = ((i + t) & 7) as i32;
        let level = (k - 4).unsigned_abs() as u16; // 4,3,2,1,0,1,2,3
        set_palette(
            C_WATER + i as u8,
            rgb(2 + level, 8 + 2 * level, 20 + level * 2),
        );
    }
}

/// Rotate the title rainbow by `t` steps (the title; play draws none of it).
fn animate_rainbow(t: u32) {
    for i in 0..8u32 {
        let hue = (i + t / 4) & 7;
        let c = match hue {
            0 => rgb(31, 6, 6),
            1 => rgb(31, 18, 4),
            2 => rgb(30, 30, 5),
            3 => rgb(8, 28, 8),
            4 => rgb(5, 26, 28),
            5 => rgb(6, 10, 31),
            6 => rgb(18, 7, 30),
            _ => rgb(30, 8, 22),
        };
        set_palette(C_RAINBOW + i as u8, c);
    }
}

/// What one of the two pages shows right now.
struct Shown {
    /// The material drawn in each grid cell (its colour follows from the cell's
    /// position), or [`UNKNOWN`].
    cells: [[u8; GW]; GH],
    /// The centre cell of the cursor box drawn on top of the grid.
    cursor: Option<(usize, usize)>,
    /// The HUD background and the static "T" are drawn.
    hud_ready: bool,
    swatch: u8,
    name: [u8; NAME_LEN],
    count: [u8; 4],
    frame: [u8; 5],
    faucet: [u8; 6],
}

const NAME_LEN: usize = 5;

impl Shown {
    const fn new() -> Self {
        Shown {
            cells: [[UNKNOWN; GW]; GH],
            cursor: None,
            hud_ready: false,
            swatch: UNKNOWN,
            name: [UNKNOWN; NAME_LEN],
            count: [UNKNOWN; 4],
            frame: [UNKNOWN; 5],
            faucet: [UNKNOWN; 6],
        }
    }

    /// The cursor box painted over the grid, so the cells under it are no longer
    /// what `cells` says: forget them and let `render` redraw them.
    fn forget_cursor(&mut self) {
        if let Some((cx, cy)) = self.cursor.take() {
            for row in &mut self.cells[cy - 1..=cy + 1] {
                row[cx - 1..=cx + 1].fill(UNKNOWN);
            }
        }
    }
}

struct Sim {
    cells: [[u8; GW]; GH],
    rng: u32,
}

impl Sim {
    const fn new() -> Self {
        Sim {
            cells: [[EMPTY; GW]; GH],
            rng: 0x1357_9BDF,
        }
    }

    fn rand_bit(&mut self) -> bool {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        x & 0x100 != 0
    }

    /// Move the cell at (x, y) to (nx, ny), putting `swap` back where it was.
    fn relocate(&mut self, x: usize, y: usize, nx: usize, ny: usize, swap: u8) {
        let c = self.cells[y][x];
        self.cells[y][x] = swap;
        self.cells[ny][nx] = c | MOVED;
    }

    #[inline(never)]
    #[unsafe(link_section = ".iwram.grains_step")]
    fn step(&mut self, frame: u32) {
        for y in (0..GH - 1).rev() {
            for i in 0..GW {
                let x = if (frame as usize ^ y) & 1 == 0 {
                    i
                } else {
                    GW - 1 - i
                };
                let c = self.cells[y][x];
                if c == EMPTY || c & MOVED != 0 || c == STONE {
                    continue;
                }
                // Sand sinks through water; water only fills empty cells.
                let free = |v: u8| v == EMPTY || (c == SAND && v == WATER);
                let below = self.cells[y + 1][x];
                if free(below) {
                    self.relocate(x, y, x, y + 1, below);
                    continue;
                }
                // Try the two lower diagonals; the generator is only drawn when
                // there is a choice to make (most settled grains have none).
                let left = x > 0 && free(self.cells[y + 1][x - 1]);
                let right = x + 1 < GW && free(self.cells[y + 1][x + 1]);
                let dx: isize = match (left, right) {
                    (true, true) => {
                        if self.rand_bit() {
                            1
                        } else {
                            -1
                        }
                    }
                    (false, true) => 1,
                    (true, false) => -1,
                    (false, false) => 0,
                };
                if dx != 0 {
                    let nx = (x as isize + dx) as usize;
                    let t = self.cells[y + 1][nx];
                    self.relocate(x, y, nx, y + 1, t);
                } else if c == WATER {
                    // Pooled water spreads sideways into an empty cell.
                    let dir = if self.rand_bit() { 1 } else { -1 };
                    for d in [dir, -dir] {
                        let nx = x as isize + d;
                        if (0..GW as isize).contains(&nx) && self.cells[y][nx as usize] == EMPTY {
                            self.relocate(x, y, nx as usize, y, EMPTY);
                            break;
                        }
                    }
                }
            }
        }
    }

    /// 3x3 brush centred on (cx, cy); `None` erases.
    fn brush(&mut self, cx: usize, cy: usize, mat: Option<u8>) {
        for y in cy.saturating_sub(1)..(cy + 2).min(GH) {
            for x in cx.saturating_sub(1)..(cx + 2).min(GW) {
                match mat {
                    Some(m) if self.cells[y][x] == EMPTY => self.cells[y][x] = m,
                    Some(_) => {}
                    None => self.cells[y][x] = EMPTY,
                }
            }
        }
    }

    /// Draw the cells of the grid that differ from `shown` into `page` (clearing
    /// MOVED flags); returns the number of occupied cells. A cell is four word
    /// writes.
    #[inline(never)]
    #[unsafe(link_section = ".iwram.grains_render")]
    fn render(&mut self, page: usize, shown: &mut Shown) -> usize {
        let mut count = 0;
        for (gy, (row, drawn)) in self.cells.iter_mut().zip(&mut shown.cells).enumerate() {
            for (gx, (cell, seen)) in row.iter_mut().zip(drawn).enumerate() {
                let v = *cell & !MOVED;
                *cell = v;
                count += usize::from(v != EMPTY);
                if *seen == v {
                    continue;
                }
                *seen = v;
                let idx = match v {
                    SAND => C_SAND + ((gx * 3 + gy * 5) & 3) as u8,
                    WATER => C_WATER + ((gx + gy) & 7) as u8,
                    STONE => C_STONE + ((gx + gy) & 1) as u8,
                    _ => C_BG,
                };
                let word = u32::from(idx) * 0x0101_0101;
                let base = (page_addr(page) + gy * CELL * W + gx * CELL) as *mut u32;
                for r in 0..CELL {
                    // SAFETY: inside the page; x is a multiple of 4.
                    unsafe { write_volatile(base.add(r * W / 4), word) }
                }
            }
        }
        count
    }
}

fn mat_colour(m: u8) -> u8 {
    match m {
        SAND => C_SAND,
        WATER => C_WATER + 4,
        _ => C_STONE,
    }
}

fn title(vblank: &VBlank, input: &mut ButtonController) {
    set_page(0);
    fill_rows(0, 0, H, C_BG);
    text(0, 48, 30, 6, b"GRAINS", C_TEXT);
    text(0, 76, 120, 2, b"PRESS START", C_BLINK);
    for i in 0..8 {
        fill_rect(0, 56 + i * 16, 80, 16, 8, C_RAINBOW + i as u8);
    }
    for t in 0..TITLE_TIMEOUT {
        input.update();
        if input.is_just_pressed(Button::Start) {
            return;
        }
        vblank.wait_for_vblank();
        animate_rainbow(t);
        set_palette(
            C_BLINK,
            if t / 30 % 2 == 0 {
                rgb(31, 31, 31)
            } else {
                rgb(2, 2, 6)
            },
        );
    }
}

/// Redraw the characters of one HUD field that differ from what `page` shows.
fn hud_text(page: usize, x: usize, shown: &mut [u8], now: &[u8]) {
    for (n, (&c, drawn)) in now.iter().zip(shown.iter_mut()).enumerate() {
        if *drawn != c {
            *drawn = c;
            let gx = glyph_x(x, n, 2);
            fill_rect(
                page,
                gx,
                HUD_Y + 3,
                font::WIDTH * 2,
                font::HEIGHT * 2,
                C_HUD,
            );
            glyph(page, gx, HUD_Y + 3, 2, c, C_TEXT);
        }
    }
}

/// Bring the HUD of `page` up to date; only what changed is drawn.
fn hud(page: usize, shown: &mut Shown, mat: usize, count: usize, frame: u32, faucet: bool) {
    if !shown.hud_ready {
        fill_rows(page, HUD_Y, H, C_HUD);
        text(page, 120, HUD_Y + 3, 2, b"T", C_TEXT);
        shown.hud_ready = true;
    }
    let colour = mat_colour(MATERIALS[mat]);
    if shown.swatch != colour {
        shown.swatch = colour;
        fill_rect(page, 4, HUD_Y + 4, 8, 8, colour);
    }
    let mut name = [b' '; NAME_LEN];
    name[..NAMES[mat].len()].copy_from_slice(NAMES[mat]);
    hud_text(page, 16, &mut shown.name, &name);
    hud_text(page, 80, &mut shown.count, &font::digits::<4>(count as u32));
    hud_text(page, 130, &mut shown.frame, &font::digits::<5>(frame));
    let label: &[u8; 6] = if faucet { b"FAUCET" } else { b"OFF   " };
    hud_text(page, 186, &mut shown.faucet, label);
}

fn cursor_box(page: usize, cx: usize, cy: usize, colour: u8) {
    let (x0, y0) = ((cx - 1) * CELL, (cy - 1) * CELL);
    let size = 3 * CELL;
    for i in 0..size {
        put_px(page, x0 + i, y0, colour);
        put_px(page, x0 + i, y0 + size - 1, colour);
        put_px(page, x0, y0 + i, colour);
        put_px(page, x0 + size - 1, y0 + i, colour);
    }
}

fn play(vblank: &VBlank, input: &mut ButtonController) -> ! {
    let mut sim = Sim::new();
    let (mut cx, mut cy) = (GW / 2, 10usize);
    let (mut mat, mut faucet, mut held) = (0usize, true, 0u32);
    let (mut front, mut count) = (0usize, 0usize);
    let mut frame = 0u32;
    let mut shown = [Shown::new(), Shown::new()];
    loop {
        input.update();
        // Move on the press, then repeat every other frame after 8 frames.
        let dpad = [Button::Left, Button::Right, Button::Up, Button::Down];
        held = if dpad.iter().any(|&b| input.is_pressed(b)) {
            held + 1
        } else {
            0
        };
        if held == 1 || (held > 8 && held % 2 == 0) {
            if input.is_pressed(Button::Left) {
                cx = (cx - 1).max(1);
            }
            if input.is_pressed(Button::Right) {
                cx = (cx + 1).min(GW - 2);
            }
            if input.is_pressed(Button::Up) {
                cy = (cy - 1).max(1);
            }
            if input.is_pressed(Button::Down) {
                cy = (cy + 1).min(GH - 2);
            }
        }
        if input.is_just_pressed(Button::R) {
            mat = (mat + 1) % MATERIALS.len();
        }
        if input.is_just_pressed(Button::L) {
            mat = (mat + MATERIALS.len() - 1) % MATERIALS.len();
        }
        if input.is_just_pressed(Button::Select) {
            faucet = !faucet;
        }
        if input.is_just_pressed(Button::Start) {
            sim = Sim::new();
        }
        if input.is_pressed(Button::A) {
            sim.brush(cx, cy, Some(MATERIALS[mat]));
        }
        if input.is_pressed(Button::B) {
            sim.brush(cx, cy, None);
        }
        if faucet && count < FAUCET_CAP {
            // Triangle sweep across columns 8..=51.
            let p = (frame / 2 % 88) as usize;
            let fx = 8 + if p < 44 { p } else { 87 - p };
            if sim.cells[0][fx] == EMPTY {
                sim.cells[0][fx] = SAND;
            }
        }
        sim.step(frame);

        let back = 1 - front;
        let on_back = &mut shown[back];
        on_back.forget_cursor();
        count = sim.render(back, on_back);
        hud(back, on_back, mat, count, frame, faucet);
        on_back.cursor = Some((cx, cy));
        cursor_box(
            back,
            cx,
            cy,
            if (frame / 8).is_multiple_of(2) {
                C_CURSOR_A
            } else {
                C_CURSOR_B
            },
        );
        vblank.wait_for_vblank();
        set_page(back);
        front = back;
        animate_water(frame);
        frame += 1;
    }
}

#[agb::entry]
fn main(_gba: agb::Gba) -> ! {
    let vblank = VBlank::get();
    let mut input = ButtonController::new();
    init_palette();
    title(&vblank, &mut input);
    play(&vblank, &mut input)
}
