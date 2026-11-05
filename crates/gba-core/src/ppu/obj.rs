//! Sprites. Filled in by a later commit.

use super::render::{Mem, OBJ_EMPTY, Regs};
use crate::SCREEN_WIDTH;

/// Draw the sprites on line `y` into `out`; returns whether any sprite pixel was drawn.
pub(super) fn render_line(
    _regs: &Regs,
    _mem: &Mem,
    _y: u32,
    out: &mut [u32; SCREEN_WIDTH],
) -> bool {
    out.fill(OBJ_EMPTY);
    false
}
