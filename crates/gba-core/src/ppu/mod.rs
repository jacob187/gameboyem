//! The PPU: display timing, the LCD I/O registers and rendering. Owned by the PPU
//! agent; the items marked **Contract** are frozen.
//!
//! Memory contract: the bus owns access semantics (mirroring, 8-bit write rules,
//! wait states) and reads/writes `palette`, `vram` and `oam` directly. The PPU
//! only reads them while rendering. I/O 0x000..0x05F (DISPCNT..BLDY) goes through
//! [`Ppu::read_io`] / [`Ppu::write_io`] at 16-bit granularity with a byte-lane mask.
//!
//! Timing contract: [`Ppu::start`] schedules the first events; the bus hands every
//! `PpuHBlank`/`PpuLineEnd` event to [`Ppu::on_event`], which reschedules the next
//! one and returns the [`PpuSignals`] for the bus to apply.

use crate::scheduler::{Event, Scheduler};
use crate::{CYCLES_PER_LINE, Framebuffer, LINES_PER_FRAME, SCREEN_HEIGHT, SCREEN_WIDTH};

/// Cycles from the start of a line to HBlank (240 dots x 4 cycles; GBATEK gives
/// 960 for the visible part, the HBlank flag itself rises a few cycles later; P1-PPU
/// decides and records it).
pub const HDRAW_CYCLES: u32 = 960;

/// What the bus must do after a PPU event. **Contract.**
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PpuSignals {
    /// IF bits to raise (VBlank / HBlank / VCount, already gated by DISPSTAT).
    pub irq: u16,
    /// Start HBlank-triggered DMA (only during VDraw lines, GBATEK).
    pub hblank_dma: bool,
    /// Start VBlank-triggered DMA.
    pub vblank_dma: bool,
    /// VBlank started: a frame is complete in `framebuffer()`.
    pub frame_done: bool,
}

pub struct Ppu {
    /// 1 KiB palette RAM (BG 0x000..0x200, OBJ 0x200..0x400). **Contract.**
    pub palette: Box<[u8; 0x400]>,
    /// 96 KiB VRAM. **Contract.**
    pub vram: Box<[u8; 0x1_8000]>,
    /// 1 KiB OAM. **Contract.**
    pub oam: Box<[u8; 0x400]>,
    /// The I/O registers 0x000..0x060 as halfwords (P1-PPU may replace this with
    /// decoded fields).
    io: [u16; 0x30],
    vcount: u16,
    front: Box<Framebuffer>,
}

impl Default for Ppu {
    fn default() -> Self {
        Self::new()
    }
}

impl Ppu {
    pub fn new() -> Self {
        Self {
            palette: Box::new([0; 0x400]),
            vram: Box::new([0; 0x1_8000]),
            oam: Box::new([0; 0x400]),
            io: [0; 0x30],
            vcount: 0,
            front: Box::new([0; SCREEN_WIDTH * SCREEN_HEIGHT]),
        }
    }

    /// Schedule the first line's events. **Contract.**
    pub fn start(&mut self, sched: &mut Scheduler) {
        sched.schedule(Event::PpuHBlank, u64::from(HDRAW_CYCLES));
        sched.schedule(Event::PpuLineEnd, u64::from(CYCLES_PER_LINE));
    }

    /// Handle a PPU event that fell due at `at`. **Contract.**
    pub fn on_event(&mut self, event: Event, at: u64, sched: &mut Scheduler) -> PpuSignals {
        let mut sig = PpuSignals::default();
        match event {
            Event::PpuHBlank => {
                // P1-PPU: render line `vcount` here when vcount < 160; DISPSTAT
                // HBlank flag/IRQ; hblank_dma on VDraw lines.
                sig.hblank_dma = u32::from(self.vcount) < crate::VISIBLE_LINES;
            }
            Event::PpuLineEnd => {
                self.vcount = (self.vcount + 1) % LINES_PER_FRAME as u16;
                if u32::from(self.vcount) == crate::VISIBLE_LINES {
                    sig.vblank_dma = true;
                    sig.frame_done = true;
                }
                let next_line = at + u64::from(CYCLES_PER_LINE);
                sched.schedule_at(Event::PpuHBlank, at + u64::from(HDRAW_CYCLES));
                sched.schedule_at(Event::PpuLineEnd, next_line);
            }
            _ => {}
        }
        sig
    }

    /// Read I/O halfword at offset `off` (even, 0x000..0x05F). **Contract.**
    pub fn read_io(&self, off: u32) -> u16 {
        match off {
            0x006 => self.vcount,
            _ => self.io[(off as usize >> 1) % self.io.len()],
        }
    }

    /// Write I/O halfword at `off`; `mask` selects the byte lanes. **Contract.**
    pub fn write_io(&mut self, off: u32, v: u16, mask: u16) {
        let i = (off as usize >> 1) % self.io.len();
        self.io[i] = (self.io[i] & !mask) | (v & mask);
    }

    /// DISPCNT (the bus needs the BG mode for VRAM 8-bit write rules). **Contract.**
    pub fn dispcnt(&self) -> u16 {
        self.io[0]
    }

    /// The last completed frame. **Contract.**
    pub fn framebuffer(&self) -> &Framebuffer {
        &self.front
    }

    /// Current scanline (VCOUNT).
    pub fn vcount(&self) -> u16 {
        self.vcount
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_228_lines_and_signals_vblank_once() {
        let mut ppu = Ppu::new();
        let mut sched = Scheduler::new();
        ppu.start(&mut sched);
        let mut frames = 0;
        let mut lines = 0;
        while sched.now() < u64::from(crate::CYCLES_PER_FRAME) {
            sched.skip_to_next();
            while let Some((ev, at)) = sched.pop_due() {
                let sig = ppu.on_event(ev, at, &mut sched);
                frames += u32::from(sig.frame_done);
                lines += u32::from(ev == Event::PpuLineEnd);
            }
        }
        assert_eq!(lines, LINES_PER_FRAME);
        assert_eq!(frames, 1);
        assert_eq!(ppu.vcount(), 0);
    }
}
