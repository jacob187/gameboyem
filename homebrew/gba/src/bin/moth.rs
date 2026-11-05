//! MOTH: a night-sky collector in tiled mode 0. Fly a moth through the dark
//! and gather drifting glints before the 30 second round ends.
//!
//! Hardware used: four regular backgrounds (a slow star/moon layer, a faster
//! hill layer that both scroll, a fixed HUD, a text overlay), seven hardware
//! sprites with two animation frames each, the VBlank interrupt (agb's frame
//! commit waits on it and an explicit handler counts frames) and the software
//! mixer for sound effects. All art and sound is generated in code.
#![no_std]
#![no_main]

use agb::{
    display::{
        Palette16, Priority, Rgb15,
        object::{DynamicSprite16, Object, Size, SpriteVram},
        tiled::{DynamicTile16, RegularBackground, RegularBackgroundSize, TileEffect, TileFormat},
    },
    external::portable_atomic::{AtomicU32, Ordering},
    fixnum::vec2,
    input::{Button, ButtonController},
    interrupt::{Interrupt, add_interrupt_handler},
    sound::mixer::{Frequency, Mixer, SoundChannel, SoundData},
};
use gba_homebrew::font;

const ROUND_FRAMES: u32 = 30 * 60;
const TITLE_TIMEOUT: u32 = 600;
const GLINTS: usize = 5;

/// Counts VBlank interrupts; animation clock only (game logic uses its own
/// frame counters). thumbv4t has no atomic read-modify-write, so load+store.
static VBLANKS: AtomicU32 = AtomicU32::new(0);

// ---- sound: square-wave blips generated at compile time (signed 8-bit) ----

#[repr(align(4))]
struct Aligned<const N: usize>([u8; N]);

/// A blip at the mixer's 10512 Hz rate: the pitch slides linearly from `f0`
/// to `f1` Hz while the volume decays to zero.
const fn blip<const N: usize>(f0: i32, f1: i32, amp: i32) -> Aligned<N> {
    let mut out = [0u8; N];
    let mut phase: u32 = 0;
    let mut i = 0;
    while i < N {
        let f = f0 + (f1 - f0) * i as i32 / N as i32;
        phase = phase.wrapping_add((f * 65536 / 10512) as u32);
        let level = amp * (N - i) as i32 / N as i32;
        let s = if phase & 0x8000 != 0 { level } else { -level };
        out[i] = s as i8 as u8;
        i += 1;
    }
    Aligned(out)
}

static COLLECT_DATA: Aligned<1800> = blip(660, 1320, 90);
static START_DATA: Aligned<3600> = blip(440, 1760, 80);
static OVER_DATA: Aligned<4000> = blip(520, 130, 90);
// SAFETY: the data is 4-byte aligned by `Aligned` and lives for 'static.
static COLLECT: SoundData = unsafe { SoundData::new(&COLLECT_DATA.0) };
static START: SoundData = unsafe { SoundData::new(&START_DATA.0) };
static OVER: SoundData = unsafe { SoundData::new(&OVER_DATA.0) };

fn sfx(mixer: &mut Mixer, data: SoundData) {
    let _ = mixer.play_sound(SoundChannel::new(data));
}

// ---- art ----

const fn c(r: u16, g: u16, b: u16) -> Rgb15 {
    Rgb15::new(r | (g << 5) | (b << 10))
}

const fn palette(colours: &[Rgb15]) -> Palette16 {
    let mut all = [Rgb15::new(0); 16];
    let mut i = 0;
    while i < colours.len() {
        all[i] = colours[i];
        i += 1;
    }
    Palette16::new(all)
}

static SPRITE_PAL: Palette16 = palette(&[
    c(0, 0, 0),
    c(12, 8, 4),
    c(31, 29, 22),
    c(22, 18, 16),
    c(30, 30, 30),
    c(31, 27, 5),
    c(31, 31, 31),
]);
static SKY_PAL: Palette16 = palette(&[
    c(1, 2, 8),
    c(14, 14, 22),
    c(28, 28, 31),
    c(30, 29, 22),
    c(20, 19, 17),
]);
static HILL_PAL: Palette16 = palette(&[c(0, 0, 0), c(3, 9, 6), c(7, 17, 10), c(2, 6, 5)]);
static TEXT_PAL: Palette16 = palette(&[c(0, 0, 0), c(31, 31, 31), c(31, 27, 5)]);
static PROMPT_ON: Palette16 = palette(&[c(0, 0, 0), c(31, 27, 5)]);
static PROMPT_OFF: Palette16 = palette(&[c(0, 0, 0), c(6, 5, 10)]);

// Left halves of the 16x16 moth (wings up / down) and the 8x8 glint (two
// twinkle frames); mirrored when drawn. Letters index SPRITE_PAL.
const MOTH: [[&str; 16]; 2] = [
    [
        "......a.", ".....a.b", "..w...bb", ".wdw..bb", ".wwdw.bb", ".wwwdwbb", "..wwwwbb",
        "...wwwbb", "....wwbb", ".....wbb", "......bb", "......bb", ".......b", ".......b",
        "........", "........",
    ],
    [
        "......a.", ".....a.b", ".......b", "......bb", "....wwbb", "..wwwwbb", ".wwwdwbb",
        "wwwdwwbb", "wwwwwwbb", "wwdwwwbb", ".wwwwwbb", "..wwdw.b", "...ww..b", ".......b",
        "........", "........",
    ],
];
const GLINT: [[&str; 8]; 2] = [
    [
        "...y", "...y", ".yyh", "yyhh", "yyhh", ".yyh", "...y", "...y",
    ],
    [
        "....", "...y", "..yy", ".yyh", ".yyh", "..yy", "...y", "....",
    ],
];

fn sprite(rows: &[&str], size: Size) -> SpriteVram {
    let mut s = DynamicSprite16::new(size);
    s.clear(0);
    let width = rows[0].len() * 2;
    for (y, row) in rows.iter().enumerate() {
        for (x, ch) in row.bytes().enumerate() {
            let idx = match ch {
                b'b' => 1,
                b'w' => 2,
                b'd' => 3,
                b'a' => 4,
                b'y' => 5,
                b'h' => 6,
                _ => 0,
            };
            s.set_pixel(x, y, idx);
            s.set_pixel(width - 1 - x, y, idx);
        }
    }
    s.to_vram(&SPRITE_PAL)
}

fn hash(x: u32, y: u32) -> u32 {
    let mut h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^ (h >> 12)
}

fn tile(pixel: impl Fn(i32, i32) -> u8) -> DynamicTile16 {
    let mut t = DynamicTile16::new().fill_with(0);
    for y in 0..8 {
        for x in 0..8 {
            t.set_pixel(x as usize, y as usize, pixel(x, y));
        }
    }
    t
}

fn new_bg(priority: Priority) -> RegularBackground {
    RegularBackground::new(
        priority,
        RegularBackgroundSize::Background32x32,
        TileFormat::FourBpp,
    )
}

/// Far layer: scattered stars and a crescent moon (a 2x2 tile disc).
fn make_sky() -> RegularBackground {
    let mut bg = new_bg(Priority::P3);
    let stars: [DynamicTile16; 8] = core::array::from_fn(|i| {
        let (sx, sy) = (
            (hash(i as u32, 1) % 8) as i32,
            (hash(i as u32, 2) % 8) as i32,
        );
        tile(|x, y| {
            if (x, y) == (sx, sy) {
                1 + (i as u8 & 1)
            } else {
                0
            }
        })
    });
    for y in 0..20 {
        for x in 0..32 {
            let h = hash(x, y);
            if h.is_multiple_of(11) {
                let t = &stars[(h >> 8) as usize % 8];
                bg.set_tile_dynamic16((x as i32, y as i32), t, TileEffect::default());
            }
        }
    }
    for ty in 0..2 {
        for tx in 0..2 {
            let t = tile(|x, y| {
                let (px, py) = (tx * 8 + x, ty * 8 + y);
                let disc = (2 * px - 15).pow(2) + (2 * py - 15).pow(2) <= 225;
                let cut = (2 * px - 23).pow(2) + (2 * py - 11).pow(2) <= 169;
                match (disc, cut) {
                    (true, false) => 3,
                    (true, true) => 4,
                    _ => 0,
                }
            });
            bg.set_tile_dynamic16((20 + tx, 3 + ty), &t, TileEffect::default());
        }
    }
    bg
}

/// Height of the hill skyline at pixel column `x`; both triangle waves repeat
/// within the 256 px map, so it wraps seamlessly.
fn hill_top(x: i32) -> i32 {
    let t1 = (x % 128 - 64).abs();
    let t2 = (x * 3 % 64 - 32).abs();
    142 - t1 / 3 - t2 / 4
}

fn hill_pixel(px: i32, py: i32, y: i32, top: i32) -> u8 {
    if y < top {
        0
    } else if y == top {
        2
    } else if (px * 5 + py * 3) % 7 == 0 {
        3
    } else {
        1
    }
}

/// Near layer: a hill silhouette. Interior tiles share one solid tile.
fn make_hills() -> RegularBackground {
    let mut bg = new_bg(Priority::P2);
    let effect = TileEffect::new(false, false, 1);
    let solid = tile(|x, y| hill_pixel(x, y, 1000, 0));
    for col in 0..32 {
        let tops: [i32; 8] = core::array::from_fn(|i| hill_top(col * 8 + i as i32));
        let (lo, hi) = (tops.iter().min().unwrap(), tops.iter().max().unwrap());
        for row in 0..20 {
            let y0 = row * 8;
            if y0 + 7 < *lo {
                continue;
            }
            if y0 > *hi {
                bg.set_tile_dynamic16((col, row), &solid, effect);
            } else {
                let t = tile(|x, y| hill_pixel(x, y, y0 + y, tops[x as usize]));
                bg.set_tile_dynamic16((col, row), &t, effect);
            }
        }
    }
    bg
}

/// A glyph two tiles tall: the 3x5 font at 2x in an 8x16 cell.
fn glyph(ch: u8) -> [DynamicTile16; 2] {
    let rows = font::rows(ch);
    let lit = |x: i32, y: i32| {
        let (fx, fy) = ((x - 1) / 2, (y - 3) / 2);
        (1..7).contains(&x) && (3..13).contains(&y) && rows[fy as usize] & (0b100 >> fx) != 0
    };
    [
        tile(|x, y| u8::from(lit(x, y))),
        tile(|x, y| u8::from(lit(x, y + 8))),
    ]
}

fn put_text(bg: &mut RegularBackground, col: i32, row: i32, s: &[u8], palette: u8) {
    let effect = TileEffect::new(false, false, palette);
    for (i, &ch) in s.iter().enumerate() {
        if ch != b' ' {
            let g = glyph(ch);
            bg.set_tile_dynamic16((col + i as i32, row), &g[0], effect);
            bg.set_tile_dynamic16((col + i as i32, row + 1), &g[1], effect);
        }
    }
}

fn put_number(bg: &mut RegularBackground, col: i32, digits: &[[DynamicTile16; 2]; 10], s: &[u8]) {
    let effect = TileEffect::new(false, false, 2);
    for (i, &d) in s.iter().enumerate() {
        let g = &digits[(d - b'0') as usize];
        bg.set_tile_dynamic16((col + i as i32, 0), &g[0], effect);
        bg.set_tile_dynamic16((col + i as i32, 1), &g[1], effect);
    }
}

// ---- game ----

struct Rng(u32);

impl Rng {
    fn below(&mut self, n: u32) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 >> 8) % n
    }
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Title,
    Play,
    Over,
}

/// Position in 1/16 px (x) and px (y).
#[derive(Clone, Copy)]
struct Glint {
    x16: i32,
    y: i32,
}

fn respawn(rng: &mut Rng, first: bool, i: usize) -> Glint {
    let base = if first { 260 + 50 * i as i32 } else { 240 };
    Glint {
        x16: (base + rng.below(60) as i32) << 4,
        y: 24 + rng.below(96) as i32,
    }
}

#[agb::entry]
fn main(mut gba: agb::Gba) -> ! {
    // SAFETY: the handler only touches an atomic and does not allocate. The
    // handle stays bound for the whole program (dropping it would unregister).
    let _vblank = unsafe {
        add_interrupt_handler(Interrupt::VBlank, |_| {
            VBLANKS.store(VBLANKS.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        })
    };
    let mut gfx = gba.graphics.get();
    let mut mixer = gba.mixer.mixer(Frequency::Hz10512);
    let mut input = ButtonController::new();
    gfx.set_background_palettes(&[
        SKY_PAL.clone(),
        HILL_PAL.clone(),
        TEXT_PAL.clone(),
        PROMPT_ON.clone(),
    ]);

    let sky = &mut make_sky();
    let hills = &mut make_hills();
    let hud = &mut new_bg(Priority::P1);
    let digits: [[DynamicTile16; 2]; 10] = core::array::from_fn(|d| glyph(b'0' + d as u8));
    put_text(hud, 1, 0, b"SCORE", 2);
    put_text(hud, 22, 0, b"TIME", 2);
    let title = &mut new_bg(Priority::P0);
    put_text(title, 13, 4, b"MOTH", 1);
    put_text(title, 6, 8, b"COLLECT THE GLINTS", 2);
    put_text(title, 9, 12, b"PRESS START", 3);
    let over = &mut new_bg(Priority::P0);
    put_text(over, 11, 6, b"TIME UP", 1);
    put_text(over, 9, 10, b"PRESS START", 3);

    let moth_frames = [
        sprite(&MOTH[0], Size::S16x16),
        sprite(&MOTH[1], Size::S16x16),
    ];
    let glint_frames = [sprite(&GLINT[0], Size::S8x8), sprite(&GLINT[1], Size::S8x8)];

    let mut rng = Rng(0xC0FF_EE11);
    let mut glints = [Glint { x16: 0, y: 0 }; GLINTS];
    for (i, g) in glints.iter_mut().enumerate() {
        *g = respawn(&mut rng, true, i);
    }
    let (mut state, mut state_frames) = (State::Title, 0u32);
    let (mut score, mut time_left) = (0u32, ROUND_FRAMES);
    let (mut mx, mut my) = (40i32, 72i32);
    let mut scroll16 = 0u32;
    let mut shown = (u32::MAX, u32::MAX);

    loop {
        input.update();
        let clock = VBLANKS.load(Ordering::Relaxed);
        state_frames += 1;

        let start = input.is_just_pressed(Button::Start);
        match state {
            State::Title | State::Over
                if start || (state == State::Title && state_frames > TITLE_TIMEOUT) =>
            {
                state = State::Play;
                state_frames = 0;
                score = 0;
                time_left = ROUND_FRAMES;
                (mx, my) = (40, 72);
                sfx(&mut mixer, START);
            }
            State::Title => my = 72 + ((state_frames as i32 / 2) % 32 - 16).abs() - 8,
            State::Over => {}
            State::Play => {
                let v = input.vector::<i32>();
                let speed = if input.is_pressed(Button::A) { 3 } else { 2 };
                mx = (mx + v.x * speed).clamp(0, 224);
                my = (my + v.y * speed).clamp(18, 126);
                time_left -= 1;
                if time_left == 0 {
                    state = State::Over;
                    state_frames = 0;
                    sfx(&mut mixer, OVER);
                }
            }
        }

        let speed16 = match state {
            State::Over => 6,
            _ => 16 + score.min(24) as i32,
        };
        scroll16 += speed16 as u32;
        for (i, g) in glints.iter_mut().enumerate() {
            if state != State::Over {
                g.x16 -= speed16;
            }
            if state == State::Play
                && ((g.x16 >> 4) + 4 - (mx + 8)).abs() < 10
                && (g.y + 4 - (my + 8)).abs() < 10
            {
                score = (score + 1).min(999);
                sfx(&mut mixer, COLLECT);
                *g = respawn(&mut rng, false, i);
            } else if g.x16 < -(8 << 4) {
                *g = respawn(&mut rng, false, i);
            }
        }

        let seconds = time_left.div_ceil(60);
        if shown != (score, seconds) {
            shown = (score, seconds);
            put_number(hud, 7, &digits, &font::digits::<3>(score));
            put_number(hud, 27, &digits, &font::digits::<2>(seconds));
        }

        sky.set_scroll_pos((scroll16 as i32 / 48, 0));
        hills.set_scroll_pos((scroll16 as i32 >> 4, 0));

        let mut frame = gfx.frame();
        frame.set_background_palette(
            3,
            if (clock / 30).is_multiple_of(2) {
                &PROMPT_ON
            } else {
                &PROMPT_OFF
            },
        );
        sky.show(&mut frame);
        hills.show(&mut frame);
        hud.show(&mut frame);
        match state {
            State::Title => {
                title.show(&mut frame);
            }
            State::Over => {
                over.show(&mut frame);
            }
            State::Play => {}
        }
        Object::new(moth_frames[(clock / 6 % 2) as usize].clone())
            .set_pos(vec2(mx, my))
            .show(&mut frame);
        for (i, g) in glints.iter().enumerate() {
            let x = g.x16 >> 4;
            if (-8..240).contains(&x) {
                Object::new(glint_frames[((clock / 10) as usize + i) % 2].clone())
                    .set_pos(vec2(x, g.y))
                    .show(&mut frame);
            }
        }
        frame.commit();
        mixer.frame();
    }
}
