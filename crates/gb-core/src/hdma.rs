//! CGB VRAM DMA registers HDMA1-HDMA5, FF51-FF55 (Pan Docs "LCD VRAM DMA
//! Transfers"). `Hw::Cgb` only; the registers are unmapped on a DMG and in
//! DMG-compatibility mode.
//!
//! This unit holds the register file and the transfer bookkeeping. The copy and
//! the CPU stall live in `Bus` (`bus.rs`), because they tick every other
//! component while the CPU waits.
//!
//! * Source (FF51/FF52) is masked to `& 0xFFF0`; the destination (FF53/FF54) is
//!   stored as an offset into VRAM, `x & 0x1FF0` (the bus address is `0x8000 |`
//!   that). The registers are write-only and read `0xFF`.
//! * FF55 bit 7 = 0 starts a general-purpose DMA (the whole length at once);
//!   bit 7 = 1 starts an HBlank DMA (0x10 bytes per HBlank on lines 0-143).
//!   Writing bit 7 = 0 while an HBlank DMA is active cancels it instead.
//! * FF55 reads `remaining blocks - 1` in bits 0-6, with bit 7 clear while an
//!   HBlank DMA is active and set otherwise, so `0xFF` means "complete". A
//!   cancelling write loads its own length bits (see [`Hdma::write5`]).
//! * A block advances both address registers and the destination wraps inside
//!   VRAM's 13 bits; a transfer whose destination wraps stops early
//!   (Pan Docs: "if the destination overflows, the transfer stops early").

use crate::state::{StateError, StateReader, StateWriter, check};

/// Bytes moved per block.
pub(crate) const BLOCK_BYTES: u16 = 0x10;
/// Source address bits that are stored.
const SRC_MASK: u16 = 0xFFF0;
/// Destination offset bits that are stored (VRAM is 0x8000-0x9FFF).
const DST_MASK: u16 = 0x1FF0;
/// Most blocks one transfer can have (FF55 bits 0-6 plus one).
const MAX_BLOCKS: u8 = 0x80;

/// What a write to FF55 asks the bus to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Start {
    /// An active HBlank transfer was stopped; nothing else happens.
    Cancelled,
    /// Copy the whole length now.
    General,
    /// Copy one block per HBlank from now on.
    HBlank,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hdma {
    src: u16,
    /// Offset into VRAM, 0..=0x1FF0, a multiple of 0x10.
    dst: u16,
    /// Blocks still to copy (0 = a transfer completed, so FF55 reads `0xFF`).
    blocks_left: u8,
    /// An HBlank transfer is running (FF55 bit 7 reads 0).
    hblank: bool,
}

impl Hdma {
    /// Post-boot: every register reads back `0xFF`, so the stored addresses are
    /// the all-ones masked values (Pan Docs "Power Up Sequence") and no
    /// transfer is pending.
    pub(crate) fn new() -> Self {
        Self {
            src: SRC_MASK,
            dst: DST_MASK,
            blocks_left: 0,
            hblank: false,
        }
    }

    /// An HBlank transfer is running.
    pub(crate) fn hblank_active(&self) -> bool {
        self.hblank
    }

    pub(crate) fn write_src_hi(&mut self, v: u8) {
        self.src = u16::from(v) << 8 | self.src & 0x00F0;
    }

    pub(crate) fn write_src_lo(&mut self, v: u8) {
        self.src = self.src & 0xFF00 | u16::from(v) & 0x00F0;
    }

    pub(crate) fn write_dst_hi(&mut self, v: u8) {
        self.dst = (u16::from(v) << 8 | self.dst & 0x00F0) & DST_MASK;
    }

    pub(crate) fn write_dst_lo(&mut self, v: u8) {
        self.dst = (self.dst & 0xFF00 | u16::from(v) & 0x00F0) & DST_MASK;
    }

    /// FF55 as read (see the module docs).
    pub(crate) fn read5(&self) -> u8 {
        let rest = self.blocks_left.wrapping_sub(1) & 0x7F;
        if self.hblank { rest } else { 0x80 | rest }
    }

    /// CPU write to FF55. The caller does the copying.
    pub(crate) fn write5(&mut self, v: u8) -> Start {
        if v & 0x80 == 0 && self.hblank {
            // The cancelling write also loads the length bits: FF55 then reads
            // `0x80 | (v & 0x7F)`, not the blocks that were left (SameBoy; the
            // hardware-verified SameSuite `hdma_lcd_off` / `hdma_mode0` expect
            // $80 after cancelling with 0; Pan Docs says the remaining length
            // stays, DECISIONS #40).
            self.blocks_left = (v & 0x7F) + 1;
            self.hblank = false;
            return Start::Cancelled;
        }
        self.blocks_left = (v & 0x7F) + 1;
        self.hblank = v & 0x80 != 0;
        if self.hblank {
            Start::HBlank
        } else {
            Start::General
        }
    }

    /// Take the next block: returns its `(source, VRAM offset)` and advances the
    /// registers. The transfer ends when no blocks are left or the destination
    /// wraps out of VRAM.
    pub(crate) fn next_block(&mut self) -> (u16, u16) {
        debug_assert!(self.blocks_left > 0);
        let at = (self.src, self.dst);
        self.src = self.src.wrapping_add(BLOCK_BYTES);
        self.dst = (self.dst + BLOCK_BYTES) & DST_MASK;
        self.blocks_left -= 1;
        if self.blocks_left == 0 || self.dst == 0 {
            self.hblank = false;
        }
        at
    }

    /// A general-purpose transfer is over when the blocks ran out or the
    /// destination wrapped (the HBlank case clears `hblank` the same way).
    pub(crate) fn general_done(&self) -> bool {
        self.blocks_left == 0 || self.dst == 0
    }

    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.u16(self.src);
        w.u16(self.dst);
        w.u8(self.blocks_left);
        w.bool(self.hblank);
    }

    pub(crate) fn load_state(r: &mut StateReader) -> Result<Self, StateError> {
        let src = r.u16()?;
        check(src & !SRC_MASK == 0, "HDMA source")?;
        let dst = r.u16()?;
        check(dst & !DST_MASK == 0, "HDMA destination")?;
        let blocks_left = r.u8()?;
        check(blocks_left <= MAX_BLOCKS, "HDMA length")?;
        let hblank = r.bool()?;
        check(!hblank || blocks_left > 0, "HDMA active without blocks")?;
        Ok(Self {
            src,
            dst,
            blocks_left,
            hblank,
        })
    }
}
