//! The four DMA channels (0x0400_00B0..0x0400_00E0). Owned by P1-SYS.
//!
//! **Contract:** [`DmaTrigger`] and [`Dma::trigger`] — the PPU's signals and the
//! APU's FIFO requests reach DMA only through them (via the bus). The bus runs an
//! active transfer instead of the CPU, one unit at a time, charging bus cycles.

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

#[derive(Debug, Clone, Default)]
pub struct Dma {
    /// Channels with a pending or running transfer (bit n = channel n).
    active: u8,
}

impl Dma {
    /// Start every enabled channel whose start timing matches `t`. **Contract.**
    pub fn trigger(&mut self, t: DmaTrigger) {
        let _ = t; // P1-SYS
    }

    /// The highest-priority (lowest-numbered) active channel, if any.
    pub fn active_channel(&self) -> Option<usize> {
        (self.active != 0).then(|| self.active.trailing_zeros() as usize)
    }
}
