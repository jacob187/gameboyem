//! Line rendering. Filled in by the following commits; for now a line is the
//! backdrop colour.

use super::Ppu;
use crate::SCREEN_WIDTH;

impl Ppu {
    /// Draw line `y` (0..160) into the back buffer from the current state.
    pub(super) fn render_line(&mut self, y: u32) {
        let backdrop = u16::from_le_bytes([self.palette[0], self.palette[1]]) & 0x7FFF;
        let start = y as usize * SCREEN_WIDTH;
        self.back[start..start + SCREEN_WIDTH].fill(backdrop);
    }
}
