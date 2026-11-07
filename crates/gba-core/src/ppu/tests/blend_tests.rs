//! Colour special effects: alpha blending, brightness, semi-transparent sprites.

use super::*;

const BLDCNT: u32 = 0x050;
const BLDALPHA: u32 = 0x052;
const BLDY: u32 = 0x054;
const WIN0H: u32 = 0x040;
const WIN0V: u32 = 0x044;
const WININ: u32 = 0x048;
const WINOUT: u32 = 0x04A;

// BLDCNT target bits (first target 0-5, second target 8-13).
const T_BG0: u16 = 1;
const T_BG1: u16 = 2;
const T_BG2: u16 = 4;
const T_OBJ: u16 = 0x10;
const T_BD: u16 = 0x20;
const ALPHA: u16 = 1 << 6;
const BRIGHTEN: u16 = 2 << 6;
const DARKEN: u16 = 3 << 6;
const OBJ_ON: u16 = 0x1000;

const C0: u16 = rgb_c(10, 20, 30);
const C1: u16 = rgb_c(20, 4, 8);
const C2: u16 = rgb_c(31, 31, 0);
const BACKDROP: u16 = rgb_c(0, 16, 31);

const fn rgb_c(r: u16, g: u16, b: u16) -> u16 {
    r | g << 5 | b << 10
}

/// BG0 (front, colour C0) over BG1 (C1) over BG2 (C2) over a backdrop.
fn stack() -> Rig {
    let mut rig = Rig::new();
    rig.pal(0, BACKDROP);
    rig.pal(20, C0);
    rig.pal(21, C1);
    rig.pal(22, C2);
    solid_bg(&mut rig, 0, 20, 0);
    solid_bg(&mut rig, 1, 21, 1);
    solid_bg(&mut rig, 2, 22, 2);
    rig
}

fn channels(c: u16) -> [u32; 3] {
    [
        u32::from(c & 31),
        u32::from(c >> 5 & 31),
        u32::from(c >> 10 & 31),
    ]
}

fn from_channels(c: [u32; 3]) -> u16 {
    (c[0] | c[1] << 5 | c[2] << 10) as u16
}

fn alpha(a: u16, b: u16, eva: u32, evb: u32) -> u16 {
    let (a, b) = (channels(a), channels(b));
    from_channels([0, 1, 2].map(|i| ((a[i] * eva.min(16) + b[i] * evb.min(16)) / 16).min(31)))
}

fn brighten(c: u16, evy: u32) -> u16 {
    let c = channels(c);
    from_channels(c.map(|v| v + (31 - v) * evy.min(16) / 16))
}

fn darken(c: u16, evy: u32) -> u16 {
    let c = channels(c);
    from_channels(c.map(|v| v - v * evy.min(16) / 16))
}

#[test]
fn alpha_blends_the_top_first_target_with_the_second_target_below() {
    let mut rig = stack();
    rig.write(BLDCNT, T_BG0 | ALPHA | T_BG1 << 8);
    rig.write(BLDALPHA, 12 | 5 << 8);
    rig.write(DISPCNT, 0x0300);
    assert_eq!(px(&rig.frame(), 50, 50), alpha(C0, C1, 12, 5));
    // Exact: r = (10*12 + 20*5) / 16 = 13, g = (20*12 + 4*5) / 16 = 16, b = (30*12 + 8*5) / 16 = 25.
    assert_eq!(alpha(C0, C1, 12, 5), rgb(13, 16, 25));
    assert_eq!(px(&rig.frame(), 50, 50), rgb(13, 16, 25));
}

#[test]
fn alpha_coefficients_clamp_at_16_and_sums_saturate() {
    let mut rig = stack();
    rig.write(BLDCNT, T_BG0 | ALPHA | T_BG1 << 8);
    rig.write(DISPCNT, 0x0300);
    // 31 behaves as 16: (a + b) saturating at 31.
    rig.write(BLDALPHA, 31 | 31 << 8);
    assert_eq!(px(&rig.frame(), 50, 50), alpha(C0, C1, 16, 16));
    assert_eq!(px(&rig.frame(), 50, 50), rgb(30, 24, 31));
    // EVA = 0, EVB = 16: only the second target shows.
    rig.write(BLDALPHA, 16 << 8);
    assert_eq!(px(&rig.frame(), 50, 50), C1);
    // EVA = 16, EVB = 0: only the first.
    rig.write(BLDALPHA, 16);
    assert_eq!(px(&rig.frame(), 50, 50), C0);
}

#[test]
fn alpha_needs_a_first_target_on_top_and_a_second_target_right_below() {
    let mut rig = stack();
    rig.write(BLDALPHA, 8 | 8 << 8);
    rig.write(DISPCNT, 0x0300);
    // Top (BG0) is not a first target.
    rig.write(BLDCNT, T_BG1 | ALPHA | T_BG1 << 8);
    assert_eq!(px(&rig.frame(), 50, 50), C0);
    // The layer below is not a second target.
    rig.write(BLDCNT, T_BG0 | ALPHA | T_BG0 << 8);
    assert_eq!(px(&rig.frame(), 50, 50), C0);
    // BG2 is a second target but BG1 sits between: only the next layer counts.
    rig.write(DISPCNT, 0x0700);
    rig.write(BLDCNT, T_BG0 | ALPHA | T_BG2 << 8);
    assert_eq!(px(&rig.frame(), 50, 50), C0);
    // Effect select 0: nothing, whatever the targets.
    rig.write(BLDCNT, T_BG0 | T_BG1 << 8);
    assert_eq!(px(&rig.frame(), 50, 50), C0);
}

#[test]
fn the_backdrop_is_a_valid_second_target_but_never_blends_as_a_top() {
    let mut rig = stack();
    rig.write(BLDALPHA, 8 | 8 << 8);
    // Only BG0 is on: the backdrop is the layer below.
    rig.write(DISPCNT, 0x0100);
    rig.write(BLDCNT, T_BG0 | ALPHA | T_BD << 8);
    assert_eq!(px(&rig.frame(), 50, 50), alpha(C0, BACKDROP, 8, 8));
    // A backdrop pixel (no BG on) has nothing below it, so even with the backdrop as
    // both targets alpha does nothing.
    rig.write(DISPCNT, 0);
    rig.write(BLDCNT, T_BD | ALPHA | T_BD << 8);
    assert_eq!(px(&rig.frame(), 50, 50), BACKDROP);
}

#[test]
fn brightness_increase_and_decrease() {
    let mut rig = stack();
    rig.write(DISPCNT, 0x0300);
    for evy in [0u16, 1, 5, 8, 15, 16, 20, 31] {
        rig.write(BLDY, evy);
        rig.write(BLDCNT, T_BG0 | BRIGHTEN);
        assert_eq!(
            px(&rig.frame(), 50, 50),
            brighten(C0, evy.into()),
            "up {evy}"
        );
        rig.write(BLDCNT, T_BG0 | DARKEN);
        assert_eq!(
            px(&rig.frame(), 50, 50),
            darken(C0, evy.into()),
            "down {evy}"
        );
    }
    // Exact values: EVY = 8: r = 10 + 21*8/16 = 20, down: 10 - 10*8/16 = 5.
    rig.write(BLDY, 8);
    rig.write(BLDCNT, T_BG0 | BRIGHTEN);
    assert_eq!(px(&rig.frame(), 50, 50), rgb(20, 25, 30));
    rig.write(BLDCNT, T_BG0 | DARKEN);
    assert_eq!(px(&rig.frame(), 50, 50), rgb(5, 10, 15));
    // 16 (and above) is full white / black.
    rig.write(BLDY, 31);
    rig.write(BLDCNT, T_BG0 | BRIGHTEN);
    assert_eq!(px(&rig.frame(), 50, 50), 0x7FFF);
    rig.write(BLDCNT, T_BG0 | DARKEN);
    assert_eq!(px(&rig.frame(), 50, 50), 0);
}

#[test]
fn brightness_applies_to_selected_first_targets_only_including_the_backdrop() {
    let mut rig = stack();
    rig.write(BLDY, 8);
    rig.write(DISPCNT, 0x0300);
    // BG1 is selected but BG0 is on top: unchanged.
    rig.write(BLDCNT, T_BG1 | BRIGHTEN);
    assert_eq!(px(&rig.frame(), 50, 50), C0);
    // Second-target bits are irrelevant for brightness.
    rig.write(BLDCNT, T_BG0 | BRIGHTEN | (T_BG1 | T_BG2) << 8);
    assert_eq!(px(&rig.frame(), 50, 50), brighten(C0, 8));
    // Backdrop pixels (nothing drawn there).
    rig.write(DISPCNT, 0);
    rig.write(BLDCNT, T_BD | DARKEN);
    assert_eq!(px(&rig.frame(), 50, 50), darken(BACKDROP, 8));
    rig.write(BLDCNT, T_BG0 | DARKEN);
    assert_eq!(px(&rig.frame(), 50, 50), BACKDROP);
}

/// A 16x16 sprite of palette index 3 (OBJ palette) at (40, 30), `mode` in attr0
/// bits 10-11, over the BG stack.
fn with_sprite(rig: &mut Rig, mode: u16, priority: u16) {
    rig.hide_all_sprites();
    rig.pal(256 + 3, rgb(1, 30, 12));
    for i in 0..64usize {
        let a = 0x1_0000 + (1 + i) * 32;
        rig.ppu.vram[a..a + 32].fill(0x33);
    }
    rig.oam(0, 30 | mode << 10, 40 | 1 << 14, 1 | priority << 10);
}

const SPRITE: u16 = rgb_c(1, 30, 12);

#[test]
fn semi_transparent_sprites_blend_with_a_valid_second_target_whatever_bldcnt_says() {
    let mut rig = stack();
    with_sprite(&mut rig, 1, 0);
    rig.write(BLDALPHA, 6 | 10 << 8);
    rig.write(DISPCNT, 0x0300 | OBJ_ON | 0x40);
    // No effect selected, BG0 is the second target: alpha anyway.
    rig.write(BLDCNT, T_BG0 << 8);
    assert_eq!(px(&rig.frame(), 45, 35), alpha(SPRITE, C0, 6, 10));
    // The OBJ first-target bit is not needed, a brightness mode does not matter.
    rig.write(BLDCNT, BRIGHTEN | T_OBJ | T_BG0 << 8);
    rig.write(BLDY, 16);
    assert_eq!(px(&rig.frame(), 45, 35), alpha(SPRITE, C0, 6, 10));
    // Outside the sprite BG0 is untouched (BG0 is not a first target).
    assert_eq!(px(&rig.frame(), 100, 100), C0);
}

#[test]
fn semi_transparent_sprites_fall_back_to_the_normal_path_without_a_second_target() {
    let mut rig = stack();
    with_sprite(&mut rig, 1, 0);
    rig.write(BLDALPHA, 6 | 10 << 8);
    rig.write(DISPCNT, 0x0300 | OBJ_ON | 0x40);
    // The layer below (BG0) is not a second target: a plain sprite.
    rig.write(BLDCNT, T_BG1 << 8);
    assert_eq!(px(&rig.frame(), 45, 35), SPRITE);
    // ... which can still be brightened as an ordinary first target.
    rig.write(BLDY, 8);
    rig.write(BLDCNT, T_OBJ | BRIGHTEN | T_BG1 << 8);
    assert_eq!(px(&rig.frame(), 45, 35), brighten(SPRITE, 8));
    // Over the backdrop alone: a valid second target when bit 13 is set.
    rig.write(DISPCNT, OBJ_ON | 0x40);
    rig.write(BLDCNT, T_BD << 8);
    assert_eq!(px(&rig.frame(), 45, 35), alpha(SPRITE, BACKDROP, 6, 10));
}

#[test]
fn a_normal_sprite_is_a_first_target_like_any_layer() {
    let mut rig = stack();
    with_sprite(&mut rig, 0, 0);
    rig.write(BLDALPHA, 8 | 8 << 8);
    rig.write(DISPCNT, 0x0300 | OBJ_ON | 0x40);
    rig.write(BLDCNT, T_OBJ | ALPHA | T_BG0 << 8);
    assert_eq!(px(&rig.frame(), 45, 35), alpha(SPRITE, C0, 8, 8));
    rig.write(BLDCNT, ALPHA | T_BG0 << 8);
    assert_eq!(px(&rig.frame(), 45, 35), SPRITE);
}

#[test]
fn a_sprite_behind_a_bg_can_be_the_second_target() {
    let mut rig = stack();
    // The sprite has priority 1: BG0 (priority 0) is in front of it.
    with_sprite(&mut rig, 0, 1);
    rig.write(BLDALPHA, 8 | 8 << 8);
    rig.write(DISPCNT, 0x0300 | OBJ_ON | 0x40);
    rig.write(BLDCNT, T_BG0 | ALPHA | T_OBJ << 8);
    assert_eq!(px(&rig.frame(), 45, 35), alpha(C0, SPRITE, 8, 8));
}

#[test]
fn window_effect_bit_turns_effects_off_per_region() {
    let mut rig = stack();
    rig.write(BLDCNT, T_BG0 | ALPHA | T_BG1 << 8);
    rig.write(BLDALPHA, 8 | 8 << 8);
    rig.write(WIN0H, 100);
    rig.write(WIN0V, 160);
    // Inside WIN0: BG0 and BG1 but no effects. Outside: effects (bit 5).
    rig.write(WININ, T_BG0 | T_BG1);
    rig.write(WINOUT, T_BG0 | T_BG1 | 0x20);
    rig.write(DISPCNT, 0x0300 | 1 << 13);
    let f = rig.frame();
    assert_eq!(px(&f, 50, 50), C0, "inside: effects disabled");
    assert_eq!(px(&f, 150, 50), alpha(C0, C1, 8, 8), "outside: effects on");
    // And with the bit set inside, both blend.
    rig.write(WININ, T_BG0 | T_BG1 | 0x20);
    let f = rig.frame();
    assert_eq!(px(&f, 50, 50), alpha(C0, C1, 8, 8));
}

#[test]
fn brightness_obeys_the_window_effect_bit_too() {
    let mut rig = stack();
    rig.write(BLDCNT, T_BG0 | BRIGHTEN);
    rig.write(BLDY, 16);
    rig.write(WIN0H, 100);
    rig.write(WIN0V, 160);
    rig.write(WININ, T_BG0);
    rig.write(WINOUT, T_BG0 | 0x20);
    rig.write(DISPCNT, 0x0100 | 1 << 13);
    let f = rig.frame();
    assert_eq!(px(&f, 50, 50), C0);
    assert_eq!(px(&f, 150, 50), 0x7FFF);
}

#[test]
fn semi_transparent_sprites_are_also_gated_by_the_window_effect_bit() {
    // An approximation: GBATEK does not spell out whether the window's effect bit
    // also disables the semi-transparent blend; here it does.
    let mut rig = stack();
    with_sprite(&mut rig, 1, 0);
    rig.write(BLDALPHA, 8 | 8 << 8);
    rig.write(BLDCNT, T_BG0 << 8);
    rig.write(WIN0H, 100);
    rig.write(WIN0V, 160);
    rig.write(WININ, T_BG0 | T_OBJ);
    rig.write(WINOUT, T_BG0 | T_OBJ | 0x20);
    rig.write(DISPCNT, 0x0100 | OBJ_ON | 0x40 | 1 << 13);
    assert_eq!(px(&rig.frame(), 45, 35), SPRITE);
    rig.write(WIN0H, 40 << 8 | 56);
    rig.write(WIN0V, 30 << 8 | 46);
    rig.write(WININ, T_BG0 | T_OBJ | 0x20);
    assert_eq!(px(&rig.frame(), 45, 35), alpha(SPRITE, C0, 8, 8));
}

#[test]
fn blend_registers_read_back_masked() {
    let mut rig = Rig::new();
    rig.write(BLDALPHA, 0xFFFF);
    assert_eq!(rig.ppu.read_io(BLDALPHA), 0x1F1F);
    rig.write(BLDY, 0xFFFF);
    assert_eq!(rig.ppu.read_io(BLDY), 0, "BLDY is write-only");
}
