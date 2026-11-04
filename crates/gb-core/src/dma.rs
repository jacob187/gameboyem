//! OAM DMA (Pan Docs: "OAM DMA Transfer"): writing `XX` to 0xFF46 copies
//! `XX00..=XX9F` into OAM, one byte per M-cycle.
//!
//! Timing, in M-cycles relative to the CPU's write of 0xFF46 (verified against
//! mooneye `oam_dma_start`, `oam_dma_timing`, `oam_dma_restart`):
//!
//! * `+0` write.
//! * `+1` nothing yet: OAM and the buses are still accessible. If an earlier
//!   transfer is running it keeps copying and keeps the buses locked.
//! * `+2 ..= +161` the new transfer copies byte `0 ..= 159`; during these
//!   cycles OAM reads `0xFF` and the source bus is in conflict with the CPU.
//! * `+162` everything is accessible again.
//!
//! "During a cycle" means: the component tick runs first, then the CPU access
//! (see `bus.rs`), so [`Dma::is_blocking`] describes the cycle whose tick just ran.

use crate::state::{StateError, StateReader, StateWriter, check};

/// Number of bytes in OAM.
const OAM_LEN: u8 = 160;
/// M-cycles between the register write and the first copy.
const START_DELAY: u8 = 2;

#[derive(Clone, Copy, Debug)]
struct Transfer {
    source: u16,
    index: u8,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    source: u16,
    delay: u8,
}

#[derive(Clone, Debug, Default)]
pub struct Dma {
    reg: u8,
    active: Option<Transfer>,
    pending: Option<Pending>,
    /// A byte was copied during the most recent tick (bus locked this cycle).
    busy: bool,
    /// Whether the most recent copy's source is on the video bus (VRAM); every
    /// other source (ROM, cartridge RAM, WRAM) is on the external bus. Only
    /// meaningful while `busy`.
    video_bus: bool,
    /// Last byte the DMA engine moved; what a conflicting CPU read observes.
    data_byte: u8,
}

impl Dma {
    pub fn new() -> Self {
        Self {
            reg: 0xFF,
            ..Self::default()
        }
    }

    /// CGB post-boot DMA register: 0x00 (Pan Docs "Power Up Sequence").
    pub fn new_cgb() -> Self {
        Self::default()
    }

    pub fn read_reg(&self) -> u8 {
        self.reg
    }

    /// CPU write to 0xFF46. A running transfer is *not* stopped immediately; it
    /// is replaced when the new one starts two M-cycles later.
    pub fn start(&mut self, value: u8) {
        self.reg = value;
        self.pending = Some(Pending {
            source: (value as u16) << 8,
            delay: START_DELAY,
        });
    }

    /// True during a cycle in which the DMA engine is using the bus: OAM is
    /// inaccessible and the source bus conflicts with the CPU.
    pub fn is_blocking(&self) -> bool {
        self.busy
    }

    /// True if a transfer is running or about to start.
    #[cfg(test)]
    fn is_active(&self) -> bool {
        self.active.is_some() || self.pending.is_some()
    }

    /// Whether a CPU access to `addr` (below 0xFE00) collides with the DMA
    /// engine this cycle. HRAM, I/O and OAM (handled separately) are not covered.
    pub fn conflicts_with(&self, addr: u16) -> bool {
        if !self.busy || addr >= 0xFE00 {
            return false;
        }
        let video = (0x8000..=0x9FFF).contains(&addr);
        video == self.video_bus
    }

    /// The byte the DMA engine last moved (what a conflicting read returns).
    pub fn data_byte(&self) -> u8 {
        self.data_byte
    }

    /// Record the byte read from the source for the copy just requested.
    pub fn latch_data(&mut self, byte: u8) {
        self.data_byte = byte;
    }

    /// Advance one M-cycle. Returns `(source_addr, oam_index)` when a byte
    /// should be copied this cycle.
    pub fn tick_mcycle(&mut self) -> Option<(u16, u8)> {
        if let Some(p) = &mut self.pending {
            p.delay -= 1;
            if p.delay == 0 {
                self.active = Some(Transfer {
                    source: p.source,
                    index: 0,
                });
                self.pending = None;
            }
        }
        self.busy = false;
        let t = self.active.as_mut()?;
        let copy = (t.source.wrapping_add(t.index as u16), t.index);
        t.index += 1;
        self.video_bus = (0x80..=0x9F).contains(&(t.source >> 8));
        if t.index == OAM_LEN {
            self.active = None;
        }
        self.busy = true;
        Some(copy)
    }
}

impl Dma {
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.u8(self.reg);
        w.bool(self.active.is_some());
        if let Some(t) = self.active {
            w.u16(t.source);
            w.u8(t.index);
        }
        w.bool(self.pending.is_some());
        if let Some(p) = self.pending {
            w.u16(p.source);
            w.u8(p.delay);
        }
        w.bool(self.busy);
        w.bool(self.video_bus);
        w.u8(self.data_byte);
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let reg = r.u8()?;
        let active = if r.bool()? {
            let t = Transfer {
                source: r.u16()?,
                index: r.u8()?,
            };
            check(t.index < OAM_LEN, "dma index")?;
            Some(t)
        } else {
            None
        };
        let pending = if r.bool()? {
            let p = Pending {
                source: r.u16()?,
                delay: r.u8()?,
            };
            check((1..=START_DELAY).contains(&p.delay), "dma delay")?;
            Some(p)
        } else {
            None
        };
        *self = Self {
            reg,
            active,
            pending,
            busy: r.bool()?,
            video_bus: r.bool()?,
            data_byte: r.u8()?,
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `n` ticks, returning the copies made.
    fn run(d: &mut Dma, n: usize) -> Vec<Option<(u16, u8)>> {
        (0..n).map(|_| d.tick_mcycle()).collect()
    }

    #[test]
    fn idle_does_nothing() {
        let mut d = Dma::new();
        assert!(run(&mut d, 10).iter().all(Option::is_none));
        assert!(!d.is_blocking());
        assert!(!d.is_active());
        assert_eq!(d.read_reg(), 0xFF);
    }

    #[test]
    fn first_copy_two_cycles_after_write_and_160_total() {
        let mut d = Dma::new();
        d.start(0xC1);
        assert_eq!(d.read_reg(), 0xC1);
        assert_eq!(d.tick_mcycle(), None, "+1: nothing yet");
        assert!(!d.is_blocking(), "+1: bus still free");
        assert_eq!(d.tick_mcycle(), Some((0xC100, 0)), "+2: first byte");
        assert!(d.is_blocking());
        let mut n = 1;
        let mut last = 0;
        while let Some((src, i)) = d.tick_mcycle() {
            assert_eq!(src, 0xC100 + i as u16);
            assert!(d.is_blocking());
            n += 1;
            last = i;
        }
        assert_eq!((n, last), (160, 159));
        assert!(!d.is_blocking());
        assert!(!d.is_active());
    }

    #[test]
    fn blocking_window_is_cycles_2_to_161() {
        let mut d = Dma::new();
        d.start(0x80);
        let window: Vec<bool> = (1..=163)
            .map(|_| {
                d.tick_mcycle();
                d.is_blocking()
            })
            .collect();
        for (i, b) in window.iter().enumerate() {
            let cycle = i + 1;
            assert_eq!(*b, (2..=161).contains(&cycle), "cycle +{cycle}");
        }
    }

    #[test]
    fn restart_keeps_old_transfer_running_then_replaces_it() {
        let mut d = Dma::new();
        d.start(0xC0);
        run(&mut d, 11); // +1 .. +11: bytes 0..=9 copied
        assert!(d.is_blocking());
        d.start(0xD0); // restart at +11
        // +1 after restart: old transfer still copies (byte 10), bus locked.
        assert_eq!(d.tick_mcycle(), Some((0xC000 + 10, 10)));
        assert!(d.is_blocking());
        // +2: new transfer begins at byte 0 from the new source.
        assert_eq!(d.tick_mcycle(), Some((0xD000, 0)));
        let rest = run(&mut d, 200);
        assert_eq!(rest.iter().flatten().count(), 159);
        assert!(!d.is_blocking());
    }

    #[test]
    fn restart_end_is_161_cycles_after_second_write() {
        let mut d = Dma::new();
        d.start(0x80);
        run(&mut d, 30);
        d.start(0x80);
        let window: Vec<bool> = (1..=165)
            .map(|_| {
                d.tick_mcycle();
                d.is_blocking()
            })
            .collect();
        assert!(window[0], "+1: old transfer keeps the bus locked");
        assert!(window[160], "+161 still blocked");
        assert!(!window[161], "+162 free");
    }

    #[test]
    fn restart_after_completion_behaves_like_fresh_start() {
        let mut d = Dma::new();
        d.start(0x80);
        run(&mut d, 170);
        d.start(0x90);
        assert_eq!(d.tick_mcycle(), None);
        assert!(!d.is_blocking());
        assert_eq!(d.tick_mcycle(), Some((0x9000, 0)));
    }

    #[test]
    fn source_bus_selects_conflict_region() {
        let mut d = Dma::new();
        d.start(0x80); // VRAM source: video bus
        run(&mut d, 2);
        assert!(d.conflicts_with(0x8000));
        assert!(d.conflicts_with(0x9FFF));
        assert!(!d.conflicts_with(0x0000));
        assert!(!d.conflicts_with(0xC000));
        assert!(!d.conflicts_with(0xFE00), "OAM is handled by the mmu");
        assert!(!d.conflicts_with(0xFF80));

        let mut d = Dma::new();
        d.start(0xC0); // WRAM source: external bus
        run(&mut d, 2);
        assert!(d.conflicts_with(0x0000));
        assert!(d.conflicts_with(0x7FFF));
        assert!(d.conflicts_with(0xA000));
        assert!(d.conflicts_with(0xDFFF));
        assert!(d.conflicts_with(0xFDFF));
        assert!(!d.conflicts_with(0x8000), "VRAM is on its own bus");
        assert!(!d.conflicts_with(0xFF00));
    }

    #[test]
    fn no_conflict_before_first_copy_or_after_end() {
        let mut d = Dma::new();
        d.start(0xC0);
        d.tick_mcycle();
        assert!(!d.conflicts_with(0xC000));
        run(&mut d, 161);
        assert!(!d.conflicts_with(0xC000));
    }
}
