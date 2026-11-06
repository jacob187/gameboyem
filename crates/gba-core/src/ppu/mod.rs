//! The PPU: display timing, the LCD I/O registers and rendering. Owned by the PPU
//! agent; the items marked **Contract** are frozen.
//!
//! Memory contract: the bus owns access semantics (mirroring, 8-bit write rules,
//! wait states) and reads/writes `palette`, `vram` and `oam` directly. The PPU
//! only reads them while rendering. I/O 0x000..0x05F (DISPCNT..BLDY) goes through
//! [`Ppu::read_io`] / [`Ppu::write_io`] at 16-bit granularity with a byte-lane mask.
//!
//! Timing contract: [`Ppu::start`] schedules the first events; the bus hands every
//! PPU event (`PpuFirstLine`, `PpuRender`, `PpuHBlank`, `PpuLineEnd`) to
//! [`Ppu::on_event`], which reschedules the next ones and returns the
//! [`PpuSignals`] for the bus to apply.

use crate::scheduler::{Event, Scheduler};
use crate::{CYCLES_PER_LINE, Framebuffer, LINES_PER_FRAME, SCREEN_HEIGHT, SCREEN_WIDTH};

mod bg;
mod effects;
mod obj;
mod render;
#[cfg(test)]
mod tests;
mod window;

/// Cycles from the start of a line to the HBlank event: the DISPSTAT HBlank flag,
/// the HBlank IRQ and the HBlank DMA trigger happen here, at cycle 1006 (GBATEK
/// "LCD Status", measured by mGBA/NBA). DECISIONS #55.
pub const HDRAW_CYCLES: u32 = 1006;

/// Cycles from the start of a line to the render event: 240 dots x 4 = 960, where
/// the last visible pixel is drawn. The line is drawn from the state at this
/// point, so a write between 960 and 1006 shows on the next line (DECISIONS #71).
pub const RENDER_CYCLES: u32 = 960;

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
    /// The internal affine reference points `[BG2, BG3][x, y]` (20.8 fixed point,
    /// sign-extended from 28 bits): what a line is drawn from. Reloaded from
    /// BGxX/BGxY at VBlank and whenever either register is written, advanced by
    /// PB/PD after each visible line (DECISIONS #58).
    affine_latch: [[i32; 2]; 2],
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
        // The BIOS leaves the rotation/scaling matrices as the identity (PA = PD =
        // 0x100 = 1.0), which the bitmap modes 3-5 draw through. The core starts
        // after the BIOS (DECISIONS #47), so the registers start in that state.
        let mut io = [0; 0x30];
        for off in [
            AFFINE_PA[0],
            AFFINE_PA[0] + 6,
            AFFINE_PA[1],
            AFFINE_PA[1] + 6,
        ] {
            io[off >> 1] = 0x100;
        }
        Self {
            palette: Box::new([0; 0x400]),
            vram: Box::new([0; 0x1_8000]),
            oam: Box::new([0; 0x400]),
            io,
            affine_latch: [[0; 2]; 2],
            vcount: 0,
            hblank: false,
            layers: render::Layers::new(),
            back: Box::new([0; SCREEN_WIDTH * SCREEN_HEIGHT]),
            front: Box::new([0; SCREEN_WIDTH * SCREEN_HEIGHT]),
        }
    }

    /// Schedule the first line's events, relative to the current clock (line 0
    /// starts now). **Contract.**
    pub fn start(&mut self, sched: &mut Scheduler) {
        sched.schedule(Event::PpuFirstLine, 0);
        sched.schedule(Event::PpuRender, u64::from(RENDER_CYCLES));
        sched.schedule(Event::PpuHBlank, u64::from(HDRAW_CYCLES));
        sched.schedule(Event::PpuLineEnd, u64::from(CYCLES_PER_LINE));
    }

    /// Handle a PPU event that fell due at `at`. **Contract.**
    pub fn on_event(&mut self, event: Event, at: u64, sched: &mut Scheduler) -> PpuSignals {
        let mut sig = PpuSignals::default();
        let stat = self.io[STAT_INDEX];
        match event {
            Event::PpuFirstLine => sig.irq |= vcount_match_irq(stat, self.vcount),
            Event::PpuRender => {
                // The visible dots are done: draw the line from the registers and
                // memory as they are now. A write the CPU or an HBlank DMA makes after
                // this point shows on the next line.
                let line = u32::from(self.vcount);
                if line < crate::VISIBLE_LINES {
                    self.render_line(line);
                    self.advance_affine_latches();
                }
            }
            Event::PpuHBlank => {
                if u32::from(self.vcount) < crate::VISIBLE_LINES {
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
                    self.reload_affine_latches();
                    std::mem::swap(&mut self.back, &mut self.front);
                    sig.vblank_dma = true;
                    sig.frame_done = true;
                    if stat & STAT_VBLANK_IRQ != 0 {
                        sig.irq |= 1;
                    }
                }
                // The VCount IRQ is requested as the line starts, not when DISPSTAT
                // is written (DECISIONS #56, #71).
                sig.irq |= vcount_match_irq(stat, self.vcount);
                if u32::from(self.vcount) < crate::VISIBLE_LINES {
                    sched.schedule_at(Event::PpuRender, at + u64::from(RENDER_CYCLES));
                }
                sched.schedule_at(Event::PpuHBlank, at + u64::from(HDRAW_CYCLES));
                sched.schedule_at(Event::PpuLineEnd, at + u64::from(CYCLES_PER_LINE));
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
        } else if i == 0 {
            // DISPCNT bit 3 is the CGB-mode bit; only BIOS opcodes can set it
            // (GBATEK "LCD I/O Display Control"), so it always reads 0.
            mask & !DISPCNT_CGB_MODE
        } else {
            mask
        };
        self.io[i] = (self.io[i] & !mask) | (v & mask);
        // Writing BGxX / BGxY copies the new value to that internal reference point
        // at once (GBATEK "LCD I/O BG Rotation/Scaling"): X and Y independently.
        for (n, &base) in AFFINE_PA.iter().enumerate() {
            let off = i << 1;
            if (base + 8..base + 16).contains(&off) {
                self.reload_affine_latch(n, (off - base - 8) >> 2);
            }
        }
    }

    /// Reload one axis (0 = x, 1 = y) of BG2's (`n` = 0) or BG3's (`n` = 1) reference
    /// point from BGxX / BGxY.
    fn reload_affine_latch(&mut self, n: usize, axis: usize) {
        let at = ((AFFINE_PA[n] + 8) >> 1) + axis * 2;
        let raw = u32::from(self.io[at]) | u32::from(self.io[at + 1]) << 16;
        self.affine_latch[n][axis] = sign_extend_28(raw as i32);
    }

    fn reload_affine_latches(&mut self) {
        for n in 0..2 {
            for axis in 0..2 {
                self.reload_affine_latch(n, axis);
            }
        }
    }

    /// After a visible line: reference point += (PB, PD) (GBATEK: "The internal
    /// registers are then incremented by dmx and dmy after each scanline").
    fn advance_affine_latches(&mut self) {
        for (n, &base) in AFFINE_PA.iter().enumerate() {
            let pb = i32::from(self.io[(base + 2) >> 1] as i16);
            let pd = i32::from(self.io[(base + 6) >> 1] as i16);
            let l = &mut self.affine_latch[n];
            // The registers are 28 bits wide: re-sign-extend from bit 27 so the
            // point wraps at 2^27 like the hardware's.
            l[0] = sign_extend_28(l[0].wrapping_add(pb));
            l[1] = sign_extend_28(l[1].wrapping_add(pd));
        }
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

/// The IF bit 2 for a VCount match at the start of line `vcount`: the match flag
/// rises as VCOUNT reaches LYC, and raises the IRQ if DISPSTAT enables it.
fn vcount_match_irq(stat: u16, vcount: u16) -> u16 {
    if stat & STAT_VCOUNT_IRQ != 0 && vcount == stat >> 8 {
        1 << 2
    } else {
        0
    }
}

/// Keep the low 28 bits of `v` as a signed value: the sign bit (27) is shifted up
/// to bit 31 and back.
fn sign_extend_28(v: i32) -> i32 {
    (v << 4) >> 4
}

/// DISPCNT bit 3.
const DISPCNT_CGB_MODE: u16 = 1 << 3;
/// I/O offsets of BG2PA and BG3PA; PB/PC/PD follow, then BGxX and BGxY.
const AFFINE_PA: [usize; 2] = [0x020, 0x030];

/// Indices into `io` (halfword offset / 2).
const STAT_INDEX: usize = 0x004 >> 1;
const VCOUNT_INDEX: usize = 0x006 >> 1;
