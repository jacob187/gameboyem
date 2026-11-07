//! Windows: fill `Layers::enable` for a line. GBATEK "LCD I/O Window Feature".
//!
//! Window 0 has the highest priority, window 1 the next, the OBJ window the next, and
//! "outside" the lowest. Each window's WININ/WINOUT byte lists the layers it shows
//! (bits 0-4: BG0-3, OBJ) and whether colour effects apply (bit 5). Those bits are
//! laid out exactly like the `enable` mask. A layer shows only if DISPCNT and the
//! window both enable it; the DISPCNT half is handled where the layers are drawn.
//!
//! Edge cases. GBATEK says "Garbage values of X2>240 or X1>X2 are interpreted as
//! X2=240" (likewise Y2>160 or Y1>Y2: Y2=160). Tonc ("Windowing") and
//! hardware instead *wrap* when the right/bottom edge is below the left/top edge:
//! "everything on that line is inside the window, except the pixels between R and
//! L". We wrap (DECISIONS #59); an edge past the screen is simply clipped.

use super::render::{ENABLE_ALL, Layers, OBJ_WINDOW, Regs};
use crate::{SCREEN_HEIGHT, SCREEN_WIDTH};

const WIN0: u16 = 1 << 13;
const OBJ_WIN: u16 = 1 << 15;
/// DISPCNT bits 13-15: any window enabled.
const ANY_WINDOW: u16 = 0xE000;

/// The pixel ranges of `[0, limit)` that `lo..hi` covers, wrapping when `lo > hi`.
/// An entry with start >= end is empty.
fn spans(lo: u32, hi: u32, limit: u32) -> [(usize, usize); 2] {
    let clip = |v: u32| v.min(limit) as usize;
    if lo <= hi {
        [(clip(lo), clip(hi)), (0, 0)]
    } else {
        [(clip(lo), limit as usize), (0, clip(hi))]
    }
}

/// Fill `layers.enable` for line `y`. `obj_marks` says the OBJ line may hold OBJ
/// window marks.
pub(super) fn fill_enable(regs: &Regs, y: u32, layers: &mut Layers, obj_marks: bool) {
    let d = regs.dispcnt;
    if d & ANY_WINDOW == 0 {
        layers.enable.fill(ENABLE_ALL);
        return;
    }
    // WINOUT low byte: outside; high byte: the OBJ window.
    layers.enable.fill((regs.winout & 0x3F) as u8);
    if d & OBJ_WIN != 0 && obj_marks {
        let mask = ((regs.winout >> 8) & 0x3F) as u8;
        for (e, &o) in layers.enable.iter_mut().zip(&layers.obj) {
            if o & OBJ_WINDOW != 0 {
                *e = mask;
            }
        }
    }
    // Window 1 first so window 0 overwrites it where they overlap.
    for w in [1usize, 0] {
        if d & (WIN0 << w) == 0 {
            continue;
        }
        let (y1, y2) = (
            u32::from(regs.win_v[w] >> 8),
            u32::from(regs.win_v[w] & 0xFF),
        );
        if !spans(y1, y2, SCREEN_HEIGHT as u32)
            .iter()
            .any(|&(s, e)| (s..e).contains(&(y as usize)))
        {
            continue;
        }
        let mask = ((regs.winin >> (8 * w)) & 0x3F) as u8;
        let (x1, x2) = (
            u32::from(regs.win_h[w] >> 8),
            u32::from(regs.win_h[w] & 0xFF),
        );
        for (s, e) in spans(x1, x2, SCREEN_WIDTH as u32) {
            if s < e {
                layers.enable[s..e].fill(mask);
            }
        }
    }
}
