//! OAM DMA (Pan Docs: "OAM DMA Transfer"): writing `XX` to 0xFF46 copies
//! `XX00..=XX9F` into OAM, one byte per M-cycle, starting one M-cycle after
//! the write. 160 + 1 M-cycles total.

#[derive(Clone, Debug, Default)]
pub struct Dma {
    reg: u8,
    /// Source base of the active transfer.
    source: u16,
    /// Next byte index to copy (0..160) while active.
    index: u8,
    active: bool,
    /// M-cycles of start-up delay remaining before copying begins.
    delay: u8,
}

impl Dma {
    pub fn new() -> Self {
        Self {
            reg: 0xFF,
            ..Self::default()
        }
    }

    pub fn read_reg(&self) -> u8 {
        self.reg
    }

    pub fn start(&mut self, value: u8) {
        self.reg = value;
        self.source = (value as u16) << 8;
        self.index = 0;
        self.delay = 1;
        self.active = true;
    }

    /// True while the CPU's access to non-HRAM memory is blocked.
    pub fn is_blocking(&self) -> bool {
        self.active && self.delay == 0
    }

    /// Advance one M-cycle. Returns `(source_addr, oam_index)` when a byte
    /// should be copied this cycle.
    pub fn tick_mcycle(&mut self) -> Option<(u16, u8)> {
        if !self.active {
            return None;
        }
        if self.delay > 0 {
            self.delay -= 1;
            return None;
        }
        let i = self.index;
        self.index += 1;
        if self.index == 160 {
            self.active = false;
        }
        Some((self.source + i as u16, i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_160_bytes_after_one_cycle_delay() {
        let mut d = Dma::new();
        d.start(0xC1);
        assert_eq!(d.tick_mcycle(), None);
        let mut n = 0;
        while let Some((src, i)) = d.tick_mcycle() {
            assert_eq!(src, 0xC100 + i as u16);
            n += 1;
        }
        assert_eq!(n, 160);
        assert!(!d.is_blocking());
    }
}
