//! Interrupt controller: IE (0x0400_0200), IF (0x0400_0202), IME (0x0400_0208).
//! **Contract.** GBATEK "GBA Interrupt Control".

/// Interrupt sources, as bit numbers of IE/IF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Interrupt {
    VBlank = 0,
    HBlank = 1,
    VCount = 2,
    Timer0 = 3,
    Timer1 = 4,
    Timer2 = 5,
    Timer3 = 6,
    Serial = 7,
    Dma0 = 8,
    Dma1 = 9,
    Dma2 = 10,
    Dma3 = 11,
    Keypad = 12,
    GamePak = 13,
}

impl Interrupt {
    /// The timer interrupt for timer `n` (0..=3).
    pub fn timer(n: usize) -> Self {
        [Self::Timer0, Self::Timer1, Self::Timer2, Self::Timer3][n & 3]
    }

    /// The DMA interrupt for channel `n` (0..=3).
    pub fn dma(n: usize) -> Self {
        [Self::Dma0, Self::Dma1, Self::Dma2, Self::Dma3][n & 3]
    }
}

/// IE / IF / IME.
#[derive(Debug, Clone, Default)]
pub struct Irq {
    pub ie: u16,
    pub if_: u16,
    pub ime: bool,
}

impl Irq {
    /// Set `src`'s IF bit (a component requesting an interrupt).
    #[inline]
    pub fn raise(&mut self, src: Interrupt) {
        self.if_ |= 1 << src as u16;
    }

    /// A write to IF acknowledges: every 1 bit written clears that flag.
    pub fn write_if(&mut self, v: u16) {
        self.if_ &= !v;
    }

    /// The IRQ line into the CPU: IME set and an enabled flag raised. (The CPU
    /// also checks CPSR.I.)
    #[inline]
    pub fn line(&self) -> bool {
        self.ime && self.wakes_halt()
    }

    /// Whether halt ends: an enabled flag is raised, regardless of IME.
    #[inline]
    pub fn wakes_halt(&self) -> bool {
        self.ie & self.if_ & 0x3FFF != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_needs_ime_ie_and_if() {
        let mut irq = Irq::default();
        irq.raise(Interrupt::VBlank);
        assert!(!irq.line());
        irq.ie = 1;
        assert!(irq.wakes_halt() && !irq.line());
        irq.ime = true;
        assert!(irq.line());
        irq.write_if(1);
        assert!(!irq.line());
    }
}
