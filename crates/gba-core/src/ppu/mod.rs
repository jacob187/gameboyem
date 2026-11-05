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

mod bg;
mod obj;
mod render;
#[cfg(test)]
mod tests;

/// Cycles from the start of a line to the HBlank event: the line is rendered and
/// the DISPSTAT HBlank flag rises here. 240 dots x 4 cycles = 960 is where the
/// last visible pixel is drawn, but the flag (and the HBlank IRQ / DMA trigger)
/// rises later, at cycle 1006 (GBATEK "LCD Status", measured by mGBA/NBA). See
/// DECISIONS #55.
pub const HDRAW_CYCLES: u32 = 1006;

/// DISPSTAT bits.
const STAT_VBLANK_IRQ: u16 = 1 << 3;
const STAT_HBLANK_IRQ: u16 = 1 << 4;
const STAT_VCOUNT_IRQ: u16 = 1 << 5;
/// The writable DISPSTAT bits: the three IRQ enables and LYC (bits 8-15).
const STAT_WRITE_MASK: u16 = 0xFF38;

/// First line of VBlank and the last line whose DISPSTAT VBlank flag is set
/// (GBATEK "LCD I/O Display Status": the flag is clear again in line 227).
const VBLANK_FIRST_LINE: u16 = 160;
const VBLANK_LAST_FLAG_LINE: u16 = 226;

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
    /// The I/O registers 0x000..0x060 as halfwords, exactly as written (reads
    /// apply per-register masks, see [`Ppu::read_io`]).
    io: [u16; 0x30],
    vcount: u16,
    /// The HBlank flag: set by the HBlank event, cleared at line end.
    hblank: bool,
    /// Per-layer line buffers (scratch; see `render.rs`).
    layers: Box<render::Layers>,
    /// The frame being drawn, one line per HBlank.
    back: Box<Framebuffer>,
    /// The last complete frame (swapped in at VBlank start).
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
            hblank: false,
            layers: render::Layers::new(),
            back: Box::new([0; SCREEN_WIDTH * SCREEN_HEIGHT]),
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
        let stat = self.io[STAT_INDEX];
        match event {
            Event::PpuHBlank => {
                let line = u32::from(self.vcount);
                if line < crate::VISIBLE_LINES {
                    // The line is drawn from the state at HBlank; writes the CPU or
                    // an HBlank DMA makes after this point show on the next line.
                    self.render_line(line);
                    sig.hblank_dma = true;
                }
                self.hblank = true;
                if stat & STAT_HBLANK_IRQ != 0 {
                    sig.irq |= 1 << 1;
                }
            }
            Event::PpuLineEnd => {
                self.hblank = false;
                self.vcount = (self.vcount + 1) % LINES_PER_FRAME as u16;
                if self.vcount == VBLANK_FIRST_LINE {
                    std::mem::swap(&mut self.back, &mut self.front);
                    sig.vblank_dma = true;
                    sig.frame_done = true;
                    if stat & STAT_VBLANK_IRQ != 0 {
                        sig.irq |= 1;
                    }
                }
                // The VCount IRQ is requested as the line starts, not when DISPSTAT
                // is written (DECISIONS #56).
                if stat & STAT_VCOUNT_IRQ != 0 && self.vcount == stat >> 8 {
                    sig.irq |= 1 << 2;
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
    ///
    /// Write-only registers (scroll, affine parameters, window sizes, MOSAIC,
    /// BLDY) read as 0.
    pub fn read_io(&self, off: u32) -> u16 {
        let i = (off as usize >> 1) % self.io.len();
        match off & !1 {
            0x004 => {
                let line = self.vcount;
                let vblank = (VBLANK_FIRST_LINE..=VBLANK_LAST_FLAG_LINE).contains(&line);
                let lyc = self.io[STAT_INDEX] >> 8;
                (self.io[STAT_INDEX] & STAT_WRITE_MASK)
                    | u16::from(vblank)
                    | u16::from(self.hblank) << 1
                    | u16::from(line == lyc) << 2
            }
            0x006 => self.vcount,
            0x000 => self.io[i],
            // Bit 0 is the (undocumented) green swap flag.
            0x002 => self.io[i] & 1,
            // BG0CNT/BG1CNT have no overflow bit (bit 13), GBATEK "LCD I/O BG Control".
            0x008 | 0x00A => self.io[i] & 0xDFFF,
            0x00C | 0x00E => self.io[i],
            // WININ / WINOUT.
            0x048 | 0x04A => self.io[i] & 0x3F3F,
            // BLDCNT (bits 14-15 unused) / BLDALPHA.
            0x050 => self.io[i] & 0x3FFF,
            0x052 => self.io[i] & 0x1F1F,
            _ => 0,
        }
    }

    /// Write I/O halfword at `off`; `mask` selects the byte lanes. **Contract.**
    pub fn write_io(&mut self, off: u32, v: u16, mask: u16) {
        let i = (off as usize >> 1) % self.io.len();
        let mask = if i == STAT_INDEX {
            mask & STAT_WRITE_MASK
        } else if i == VCOUNT_INDEX {
            return;
        } else {
            mask
        };
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

/// Indices into `io` (halfword offset / 2).
const STAT_INDEX: usize = 0x004 >> 1;
const VCOUNT_INDEX: usize = 0x006 >> 1;
