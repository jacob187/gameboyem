//! The four DMA channels (0x0400_00B0..0x0400_00E0). Owned by P1-SYS. GBATEK
//! "GBA DMA Transfers".
//!
//! **Contract:** [`DmaTrigger`] and [`Dma::trigger`] — the PPU's signals and the
//! APU's FIFO requests reach DMA only through them (via the bus). The bus runs an
//! active transfer instead of the CPU ([`Bus::run_dma`]), one unit at a time:
//! each unit is a read then a write, charged N for the first unit of a transfer
//! and S for the rest. The 2-cycle start delay (4 when both ends are cartridge
//! memory) is charged when a triggered channel first runs.

use crate::bus::{Access, Bus, Width};
use crate::irq::Interrupt;
use crate::state::{StateError, check};
use emu_common::state::{StateReader, StateWriter};

/// Why a DMA might start. **Contract.**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaTrigger {
    /// Start timing 0 (immediately, after the 2-cycle start delay).
    Immediate,
    VBlank,
    HBlank,
    /// Sound FIFO A wants data (DMA1/DMA2 in special mode).
    FifoA,
    /// Sound FIFO B wants data.
    FifoB,
    /// Video capture (DMA3 special mode, lines 2..=161).
    VideoCapture,
}

const FIFO_A: u32 = 0x0400_00A0;
const FIFO_B: u32 = 0x0400_00A4;

#[derive(Debug, Clone, Copy, Default)]
pub struct Channel {
    /// The registers as written (SAD/DAD/CNT_L are write-only on hardware).
    pub sad: u32,
    pub dad: u32,
    pub count: u16,
    pub control: u16,
    /// Internal pointers and unit counter, latched from the registers.
    src: u32,
    dst: u32,
    remaining: u32,
    /// The next unit is the first of its transfer (N access, not S).
    first: bool,
    /// Triggered but its start delay hasn't been charged yet.
    fresh: bool,
}

impl Channel {
    fn timing(&self) -> u16 {
        (self.control >> 12) & 3
    }

    fn enabled(&self) -> bool {
        self.control & 0x8000 != 0
    }

    fn count_or_max(&self, n: usize) -> u32 {
        let max = if n == 3 { 0x1_0000 } else { 0x4000 };
        match u32::from(self.count) & (max - 1) {
            0 => max,
            c => c,
        }
    }
}

/// Source and destination address masks (27 bits internal-only, 28 bits any).
fn src_mask(n: usize) -> u32 {
    if n == 0 { 0x07FF_FFFF } else { 0x0FFF_FFFF }
}

fn dst_mask(n: usize) -> u32 {
    if n == 3 { 0x0FFF_FFFF } else { 0x07FF_FFFF }
}

#[derive(Debug, Clone, Default)]
pub struct Dma {
    pub ch: [Channel; 4],
    /// Channels with a pending or running transfer (bit n = channel n).
    active: u8,
    /// The last value any DMA moved: what a DMA read from an unreadable region
    /// (BIOS, unmapped) returns.
    latch: u32,
}

impl Dma {
    /// All four channels with both the visible registers and the internal latched
    /// ones, the active set and the bus latch.
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        for c in &self.ch {
            w.u32(c.sad);
            w.u32(c.dad);
            w.u16(c.count);
            w.u16(c.control);
            w.u32(c.src);
            w.u32(c.dst);
            w.u32(c.remaining);
            w.bool(c.first);
            w.bool(c.fresh);
        }
        w.u8(self.active);
        w.u32(self.latch);
    }

    /// An enabled channel has between 1 and its maximum units left (`remaining`
    /// is decremented per unit, and latched from the count on enabling, on every
    /// repeat and as 4 for a sound FIFO), and an active channel is enabled
    /// (disabling clears the active bit). `control` holds only the stored bits.
    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let mut dma = Self::default();
        for (n, c) in dma.ch.iter_mut().enumerate() {
            *c = Channel {
                sad: r.u32()?,
                dad: r.u32()?,
                count: r.u16()?,
                control: r.u16()?,
                src: r.u32()?,
                dst: r.u32()?,
                remaining: r.u32()?,
                first: r.bool()?,
                fresh: r.bool()?,
            };
            check(c.control & !control_mask(n) == 0, "DMA control")?;
            let max = if n == 3 { 0x1_0000 } else { 0x4000 };
            check(c.remaining <= max, "DMA remaining units")?;
            check(
                !c.enabled() || c.remaining >= 1,
                "enabled DMA with no units",
            )?;
        }
        dma.active = r.u8()?;
        dma.latch = r.u32()?;
        check(dma.active <= 0xF, "DMA active set")?;
        for (n, c) in dma.ch.iter().enumerate() {
            check(
                dma.active & (1 << n) == 0 || c.enabled(),
                "active DMA channel is not enabled",
            )?;
        }
        *self = dma;
        Ok(())
    }

    /// Start every enabled channel whose start timing matches `t`. **Contract.**
    pub fn trigger(&mut self, t: DmaTrigger) {
        for n in 0..4 {
            let c = &mut self.ch[n];
            if !c.enabled() || self.active & (1 << n) != 0 {
                continue;
            }
            let hit = match (c.timing(), t) {
                (0, DmaTrigger::Immediate) | (1, DmaTrigger::VBlank) | (2, DmaTrigger::HBlank) => {
                    true
                }
                (3, DmaTrigger::FifoA) => matches!(n, 1 | 2) && c.dst == FIFO_A,
                (3, DmaTrigger::FifoB) => matches!(n, 1 | 2) && c.dst == FIFO_B,
                (3, DmaTrigger::VideoCapture) => n == 3,
                _ => false,
            };
            if hit {
                c.fresh = true;
                c.first = true;
                self.active |= 1 << n;
            }
        }
    }

    /// The highest-priority (lowest-numbered) active channel, if any.
    #[inline]
    pub fn active_channel(&self) -> Option<usize> {
        (self.active != 0).then(|| self.active.trailing_zeros() as usize)
    }

    /// Video capture ends at line 162: stop DMA3 if it is in that mode.
    pub fn stop_video_capture(&mut self) {
        let c = &mut self.ch[3];
        if c.enabled() && c.timing() == 3 {
            c.control &= !0x8000;
            self.active &= !(1 << 3);
        }
    }

    /// Read the halfword at `off` (0..0x30 from 0x0400_00B0). Only CNT_H is
    /// readable; the rest read 0.
    pub fn read_reg(&self, off: u32) -> u16 {
        let n = (off / 12) as usize;
        if off % 12 == 10 {
            self.ch[n].control & control_mask(n)
        } else {
            0
        }
    }

    /// Write the halfword at `off` through a byte-lane `mask`.
    pub fn write_reg(&mut self, off: u32, v: u16, mask: u16) {
        let n = (off / 12) as usize;
        let c = &mut self.ch[n];
        let merge = |old: u16| (old & !mask) | (v & mask);
        let merge32 = |old: u32, shift: u32| {
            (old & !(u32::from(mask) << shift)) | (u32::from(v & mask) << shift)
        };
        match off % 12 {
            0 => c.sad = merge32(c.sad, 0),
            2 => c.sad = merge32(c.sad, 16),
            4 => c.dad = merge32(c.dad, 0),
            6 => c.dad = merge32(c.dad, 16),
            8 => c.count = merge(c.count),
            10 => {
                let was = c.enabled();
                c.control = merge(c.control) & control_mask(n);
                if !c.enabled() {
                    self.active &= !(1 << n);
                } else if !was {
                    // Enabling latches the registers into the internal ones.
                    c.src = c.sad & src_mask(n);
                    c.dst = c.dad & dst_mask(n);
                    c.remaining = c.count_or_max(n);
                    if c.timing() == 0 {
                        self.trigger(DmaTrigger::Immediate);
                    }
                }
            }
            _ => {}
        }
    }
}

/// CNT_H's stored bits: 5..=15, except the DMA3-only Game Pak DRQ bit 11.
fn control_mask(n: usize) -> u16 {
    if n == 3 { 0xFFE0 } else { 0xF7E0 }
}

impl Bus {
    /// Run the active DMA until it finishes, a higher-priority channel starts, or
    /// an event falls due (so the run loop can dispatch it). Does nothing if no
    /// channel is active.
    pub fn run_dma(&mut self) {
        while self.run_dma_unit() {
            if self.sched.next_at().is_some_and(|t| t <= self.sched.now()) {
                return;
            }
        }
    }

    /// Run one unit (a read and a write) of the highest-priority active channel,
    /// including its start delay if it has just been triggered. Returns false if
    /// no channel is active.
    pub fn run_dma_unit(&mut self) -> bool {
        let Some(n) = self.dma.active_channel() else {
            return false;
        };
        if self.dma.ch[n].fresh {
            self.dma_begin(n);
        }
        self.dma_unit(n);
        true
    }

    fn is_fifo(&self, n: usize) -> bool {
        let c = &self.dma.ch[n];
        // Same test as `Dma::trigger` uses to start it, so the two cannot disagree.
        matches!(n, 1 | 2) && c.timing() == 3 && (c.dst == FIFO_A || c.dst == FIFO_B)
    }

    /// First run of a triggered transfer: charge the start delay and set up the
    /// special modes.
    fn dma_begin(&mut self, n: usize) {
        let fifo = self.is_fifo(n);
        let c = &mut self.dma.ch[n];
        c.fresh = false;
        if fifo {
            // Sound DMA always moves 4 words; count and unit size are ignored.
            c.remaining = 4;
        }
        let (src, dst, count) = (c.src, c.dst, c.remaining);
        if n == 3 && self.cart.eeprom_at(dst) {
            self.cart.eeprom_begin_write_dma(count);
        }
        // 2 internal cycles, 4 if both ends are in the cartridge area.
        let cart = |a: u32| (0x08..=0x0F).contains(&(a >> 24));
        self.sched
            .advance(if cart(src) && cart(dst) { 4 } else { 2 });
    }

    /// Move one unit (a read and a write) on channel `n`.
    fn dma_unit(&mut self, n: usize) {
        let c = self.dma.ch[n];
        let fifo = self.is_fifo(n);
        let w = if fifo || c.control & 0x400 != 0 {
            Width::W
        } else {
            Width::H
        };
        let size = w as u32;
        let access = if c.first { Access::NonSeq } else { Access::Seq };
        let src = c.src & !(size - 1);
        let dst = c.dst & !(size - 1);

        let value = if (0x02..=0x0F).contains(&(src >> 24)) {
            let cycles = self.access_cost(src, w, access);
            self.sched.advance(cycles);
            let v = self.read_value(src, w);
            self.dma.latch = if w == Width::H {
                v & 0xFFFF | v << 16
            } else {
                v
            };
            v
        } else {
            // BIOS and unmapped sources can't be read by DMA: the bus latch shows.
            self.sched.advance(1);
            self.dma.latch
        };
        let cycles = self.access_cost(dst, w, access);
        self.sched.advance(cycles);
        self.write_value(dst, value, w);

        let step = |addr: u32, ctl: u16| match ctl {
            1 => addr.wrapping_sub(size),
            2 => addr,
            _ => addr.wrapping_add(size),
        };
        let src_ctl = (c.control >> 7) & 3;
        let dst_ctl = if fifo { 2 } else { (c.control >> 5) & 3 };
        let ch = &mut self.dma.ch[n];
        ch.src = step(c.src, src_ctl);
        ch.dst = step(c.dst, dst_ctl);
        ch.first = false;
        ch.remaining -= 1;
        if ch.remaining == 0 {
            self.dma_finish(n);
        }
    }

    fn dma_finish(&mut self, n: usize) {
        let fifo = self.is_fifo(n);
        let c = &mut self.dma.ch[n];
        let raise = c.control & 0x4000 != 0;
        let repeat = (c.control & 0x200 != 0 || fifo) && c.timing() != 0;
        if repeat {
            // Stays enabled; waits for the next trigger with the count reloaded
            // (and the destination too in increment+reload mode).
            c.remaining = c.count_or_max(n);
            if (c.control >> 5) & 3 == 3 {
                c.dst = c.dad & dst_mask(n);
            }
        } else {
            c.control &= !0x8000;
        }
        self.dma.active &= !(1 << n);
        if raise {
            self.irq.raise(Interrupt::dma(n));
        }
    }
}
