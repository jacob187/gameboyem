//! Window tests: WIN0, WIN1, the OBJ window and WINOUT.

use super::*;

const WIN0H: u32 = 0x040;
const WIN1H: u32 = 0x042;
const WIN0V: u32 = 0x044;
const WIN1V: u32 = 0x046;
const WININ: u32 = 0x048;
const WINOUT: u32 = 0x04A;

const WIN0_ON: u16 = 1 << 13;
const WIN1_ON: u16 = 1 << 14;
const OBJWIN_ON: u16 = 1 << 15;
const OBJ_ON: u16 = 1 << 12;

const BG0: u16 = 1;
const BG1: u16 = 2;
const OBJ: u16 = 0x10;

/// BG0 solid index 20 (front), BG1 solid index 21, backdrop colour of entry 0.
fn two_bgs() -> Rig {
    let mut rig = Rig::new();
    rig.distinct_palette();
    solid_bg(&mut rig, 0, 20, 0);
    solid_bg(&mut rig, 1, 21, 1);
    rig
}

fn bg0() -> u16 {
    pal_colour(20)
}
fn bg1() -> u16 {
    pal_colour(21)
}
fn backdrop() -> u16 {
    pal_colour(0)
}

fn win(rig: &mut Rig, h: u32, v: u32, x1: u16, x2: u16, y1: u16, y2: u16) {
    rig.write(h, x1 << 8 | x2);
    rig.write(v, y1 << 8 | y2);
}

#[test]
fn window_0_is_a_rectangle_with_exclusive_right_and_bottom_edges() {
    let mut rig = two_bgs();
    win(&mut rig, WIN0H, WIN0V, 40, 100, 20, 60);
    rig.write(WININ, BG0 | BG1);
    rig.write(WINOUT, BG1);
    rig.write(DISPCNT, 0x0300 | WIN0_ON);
    let f = rig.frame();
    for (x, y, want) in [
        (40, 20, bg0()),
        (99, 59, bg0()),
        (70, 40, bg0()),
        (39, 40, bg1()),
        (100, 40, bg1()),
        (70, 19, bg1()),
        (70, 60, bg1()),
        (0, 0, bg1()),
        (239, 159, bg1()),
    ] {
        assert_eq!(px(&f, x, y), want, "({x},{y})");
    }
}

#[test]
fn windows_are_ignored_unless_enabled_in_dispcnt() {
    let mut rig = two_bgs();
    win(&mut rig, WIN0H, WIN0V, 40, 100, 20, 60);
    rig.write(WININ, 0);
    rig.write(WINOUT, 0);
    rig.write(DISPCNT, 0x0300);
    assert_eq!(px(&rig.frame(), 70, 40), bg0());
    // Enabled, with nothing shown inside or outside: only the backdrop is left.
    rig.write(DISPCNT, 0x0300 | WIN0_ON);
    let f = rig.frame();
    assert_eq!(px(&f, 70, 40), backdrop());
    assert_eq!(px(&f, 5, 5), backdrop());
}

#[test]
fn window_layer_bits_pick_layers_per_region() {
    let mut rig = two_bgs();
    win(&mut rig, WIN0H, WIN0V, 0, 120, 0, 160);
    win(&mut rig, WIN1H, WIN1V, 120, 240, 0, 160);
    // WIN0 shows only BG1, WIN1 only BG0.
    rig.write(WININ, BG1 | BG0 << 8);
    rig.write(WINOUT, 0);
    rig.write(DISPCNT, 0x0300 | WIN0_ON | WIN1_ON);
    let f = rig.frame();
    assert_eq!(px(&f, 10, 10), bg1());
    assert_eq!(px(&f, 119, 10), bg1());
    assert_eq!(px(&f, 120, 10), bg0());
    assert_eq!(px(&f, 239, 159), bg0());
}

#[test]
fn window_priority_is_win0_then_win1_then_objwin_then_outside() {
    let mut rig = two_bgs();
    // Layers: BG0 (20), BG1 (21), plus BG2 (22) for a third marker.
    solid_bg(&mut rig, 2, 22, 2);
    // WIN0 x 0..100, WIN1 x 50..150, both y 0..80; an OBJ-window sprite covers
    // x 0..32 (y 0..32, in the WIN0 | WIN1 | outside mix).
    win(&mut rig, WIN0H, WIN0V, 0, 100, 0, 80);
    win(&mut rig, WIN1H, WIN1V, 50, 150, 0, 80);
    rig.write(WININ, BG0 | BG1 << 8);
    // WINOUT: outside = BG1 | BG2, OBJ window = BG0 | BG2.
    rig.write(WINOUT, (BG1 | 4) | (BG0 | 4) << 8);
    rig.hide_all_sprites();
    for i in 0..1024usize {
        rig.ppu.vram[0x1_0000 + i * 32..0x1_0000 + i * 32 + 32].fill(0x11);
    }
    // OBJ-window sprite 32x32 at (120, 10) and (0, 100): overlaps WIN1 only in part.
    rig.oam(0, 10 | 0x800, 120 | 2 << 14, 1);
    rig.oam(1, 100 | 0x800, 10 | 2 << 14, 1);
    rig.write(
        DISPCNT,
        0x0700 | WIN0_ON | WIN1_ON | OBJWIN_ON | OBJ_ON | 0x40,
    );
    let f = rig.frame();
    // Inside WIN0 (also inside WIN1 at x 50..100): WIN0 wins -> BG0.
    assert_eq!(px(&f, 70, 40), bg0());
    // WIN1 only -> BG1.
    assert_eq!(px(&f, 120, 40), bg1());
    // OBJ window, away from WIN0/WIN1 (sprite 0 at x 120..152 y 10..42 overlaps WIN1
    // for x < 150; x = 151 is outside WIN1): BG0 | BG2 -> BG0 is in front.
    assert_eq!(px(&f, 151, 20), bg0());
    // The OBJ window is lower than WIN1: inside WIN1 the sprite does not matter.
    assert_eq!(px(&f, 130, 20), bg1());
    // Outside every window and not under the sprite: BG1 (BG2 is behind it).
    assert_eq!(px(&f, 200, 120), bg1());
    // Sprite 1 at (10, 100): an OBJ-window region on lines with no other sprite.
    assert_eq!(px(&f, 20, 110), bg0());
    assert_eq!(px(&f, 50, 110), bg1());
    // The OBJ-window sprite draws no pixel of its own.
    assert_ne!(px(&f, 151, 20), obj_pixel_colour());
}

fn obj_pixel_colour() -> u16 {
    pal_colour(256 + 1)
}

#[test]
fn obj_window_lines_keep_their_marks_when_no_other_sprite_is_on_the_line() {
    // The review item: a line whose only sprites are OBJ-window sprites used to be
    // treated as "no sprite", wiping the window marks.
    let mut rig = two_bgs();
    rig.hide_all_sprites();
    for i in 0..1024usize {
        rig.ppu.vram[0x1_0000 + i * 32..0x1_0000 + i * 32 + 32].fill(0x11);
    }
    rig.oam(0, 50 | 0x800, 60, 1);
    rig.write(WINOUT, BG1 | BG0 << 8);
    rig.write(DISPCNT, 0x0300 | OBJWIN_ON | OBJ_ON | 0x40);
    let f = rig.frame();
    assert_eq!(px(&f, 60, 50), bg0());
    assert_eq!(px(&f, 67, 57), bg0());
    assert_eq!(px(&f, 68, 50), bg1());
    assert_eq!(px(&f, 60, 49), bg1());
    assert_eq!(px(&f, 60, 58), bg1());
}

#[test]
fn obj_window_needs_objs_enabled_and_its_own_dispcnt_bit() {
    let mut rig = two_bgs();
    rig.hide_all_sprites();
    for i in 0..1024usize {
        rig.ppu.vram[0x1_0000 + i * 32..0x1_0000 + i * 32 + 32].fill(0x11);
    }
    rig.oam(0, 50 | 0x800, 60, 1);
    rig.write(WINOUT, BG1 | BG0 << 8);
    // Bit 15 set but OBJ display off: no marks; WINOUT outside applies everywhere.
    rig.write(DISPCNT, 0x0300 | OBJWIN_ON | 0x40);
    assert_eq!(px(&rig.frame(), 60, 50), bg1());
    // OBJs on but the OBJ window bit clear and no other window: no windowing at all.
    rig.write(DISPCNT, 0x0300 | OBJ_ON | 0x40);
    assert_eq!(px(&rig.frame(), 60, 50), bg0());
}

#[test]
fn window_can_hide_sprites_and_the_obj_bit_is_per_window() {
    let mut rig = two_bgs();
    rig.hide_all_sprites();
    for i in 0..1024usize {
        rig.ppu.vram[0x1_0000 + i * 32..0x1_0000 + i * 32 + 32].fill(0x33);
    }
    // A big normal sprite (priority 0, in front of BG0 on ties) at (0, 0).
    rig.oam(0, 0, 3 << 14, 1);
    win(&mut rig, WIN0H, WIN0V, 20, 40, 0, 64);
    rig.write(WININ, BG0);
    rig.write(WINOUT, BG0 | OBJ);
    rig.write(DISPCNT, 0x0300 | WIN0_ON | OBJ_ON | 0x40);
    let f = rig.frame();
    let obj = pal_colour(256 + 3);
    assert_eq!(px(&f, 10, 10), obj, "outside: sprites allowed");
    assert_eq!(px(&f, 30, 10), bg0(), "inside WIN0: sprites hidden");
    assert_eq!(px(&f, 50, 10), obj);
}

#[test]
fn x_range_edge_cases() {
    let cases: [(u16, u16, &[(usize, bool)]); 5] = [
        // x1 > x2 wraps: [200, 240) and [0, 40).
        (
            200,
            40,
            &[
                (0, true),
                (39, true),
                (40, false),
                (199, false),
                (200, true),
                (239, true),
            ],
        ),
        // A right edge beyond the screen is clipped to 240.
        (100, 250, &[(99, false), (100, true), (239, true)]),
        // x1 == x2 is empty.
        (80, 80, &[(79, false), (80, false), (81, false)]),
        // Left edge at or beyond 240: nothing.
        (240, 255, &[(0, false), (239, false)]),
        // Full width.
        (0, 240, &[(0, true), (120, true), (239, true)]),
    ];
    for (x1, x2, probes) in cases {
        let mut rig = two_bgs();
        win(&mut rig, WIN0H, WIN0V, x1, x2, 0, 160);
        rig.write(WININ, BG0);
        rig.write(WINOUT, BG1);
        rig.write(DISPCNT, 0x0300 | WIN0_ON);
        let f = rig.frame();
        for &(x, inside) in probes {
            let want = if inside { bg0() } else { bg1() };
            assert_eq!(px(&f, x, 30), want, "x1={x1} x2={x2} at x={x}");
        }
    }
}

#[test]
fn y_range_edge_cases() {
    let cases: [(u16, u16, &[(usize, bool)]); 5] = [
        // y1 > y2 wraps: lines [120, 160) and [0, 30).
        (
            120,
            30,
            &[
                (0, true),
                (29, true),
                (30, false),
                (119, false),
                (120, true),
                (159, true),
            ],
        ),
        // A bottom edge beyond the screen is clipped to 160.
        (100, 200, &[(99, false), (100, true), (159, true)]),
        (50, 50, &[(49, false), (50, false), (51, false)]),
        (0, 160, &[(0, true), (80, true), (159, true)]),
        (200, 255, &[(0, false), (159, false)]),
    ];
    for (y1, y2, probes) in cases {
        let mut rig = two_bgs();
        win(&mut rig, WIN0H, WIN0V, 0, 240, y1, y2);
        rig.write(WININ, BG0);
        rig.write(WINOUT, BG1);
        rig.write(DISPCNT, 0x0300 | WIN0_ON);
        let f = rig.frame();
        for &(y, inside) in probes {
            let want = if inside { bg0() } else { bg1() };
            assert_eq!(px(&f, 100, y), want, "y1={y1} y2={y2} at y={y}");
        }
    }
}

#[test]
fn window_registers_are_write_only_except_winin_and_winout() {
    let mut rig = Rig::new();
    rig.write(WIN0H, 0x1234);
    assert_eq!(rig.ppu.read_io(WIN0H), 0);
    rig.write(WININ, 0xFFFF);
    rig.write(WINOUT, 0xFFFF);
    assert_eq!(rig.ppu.read_io(WININ), 0x3F3F);
    assert_eq!(rig.ppu.read_io(WINOUT), 0x3F3F);
}
