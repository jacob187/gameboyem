//! Colour special effects: alpha blending and brightness. GBATEK "LCD I/O Color
//! Special Effects".
//!
//! BLDCNT bits 0-5 pick the first target layers (BG0-3, OBJ, backdrop), bits 6-7 the
//! effect (0 none, 1 alpha, 2 brighten, 3 darken), bits 8-13 the second targets.
//! A pixel is alpha-blended when its top layer is a first target and the layer
//! below it is a second target: `I = MIN(31, I1st*EVA + I2nd*EVB)` per channel with
//! EVA/EVB clamped at 16 (/16). Brightness: `I1st + (31-I1st)*EVY` or
//! `I1st - I1st*EVY`. A semi-transparent sprite is always a first target and alpha
//! blends whenever the layer below is a second target, whatever BLDCNT's effect says
//! ("semi-transparency becomes priority", GBATEK); with no valid second target it
//! is an ordinary sprite.

use super::render::{Candidate, LAYER_BACKDROP, Regs};

pub(super) struct Blend {
    mode: u16,
    first: u8,
    second: u8,
    eva: u32,
    evb: u32,
    evy: u32,
}

/// Per-channel `(a * ea + b * eb) / 16`, capped at 31.
#[inline]
fn mix(a: u16, b: u16, ea: u32, eb: u32) -> u16 {
    let ch = |shift: u32| {
        let (x, y) = (u32::from(a >> shift) & 31, u32::from(b >> shift) & 31);
        ((x * ea + y * eb) >> 4).min(31) as u16
    };
    ch(0) | ch(5) << 5 | ch(10) << 10
}

/// `c - c * evy / 16` per channel (the literal GBATEK formula, rounding down the
/// subtracted part).
#[inline]
fn darken(c: u16, evy: u32) -> u16 {
    let ch = |shift: u32| {
        let v = u32::from(c >> shift) & 31;
        (v - ((v * evy) >> 4)) as u16
    };
    ch(0) | ch(5) << 5 | ch(10) << 10
}

impl Blend {
    pub fn new(regs: &Regs) -> Self {
        Self {
            mode: (regs.bldcnt >> 6) & 3,
            first: (regs.bldcnt & 0x3F) as u8,
            second: ((regs.bldcnt >> 8) & 0x3F) as u8,
            eva: u32::from(regs.bldalpha & 0x1F).min(16),
            evb: u32::from((regs.bldalpha >> 8) & 0x1F).min(16),
            evy: u32::from(regs.bldy & 0x1F).min(16),
        }
    }

    /// Whether any pixel of the line can change: an effect is selected, or the line
    /// holds a semi-transparent sprite.
    pub fn active(&self, semi_sprites: bool) -> bool {
        self.mode != 0 || semi_sprites
    }

    /// The final colour for a pixel whose window allows effects.
    pub fn apply(&self, top: Candidate, second: Candidate, semi: bool) -> u16 {
        let second_ok = self.second >> second.layer & 1 != 0;
        // A backdrop top has nothing below it to blend with.
        let can_alpha = second_ok && top.layer != LAYER_BACKDROP;
        if semi && can_alpha {
            return mix(top.colour, second.colour, self.eva, self.evb);
        }
        if self.first >> top.layer & 1 == 0 {
            return top.colour;
        }
        match self.mode {
            1 if can_alpha => mix(top.colour, second.colour, self.eva, self.evb),
            // c + (31 - c) * EVY / 16 == (c * (16 - EVY) + 31 * EVY) / 16 (rounded down).
            2 => mix(top.colour, 0x7FFF, 16 - self.evy, self.evy),
            3 => darken(top.colour, self.evy),
            _ => top.colour,
        }
    }
}
