//! Sound: four PSG channels, Direct Sound FIFO A/B, and the SOUNDCNT/SOUNDBIAS
//! mixer. Built in phase P2 (P2-APU). **Contract** for the bus: FIFO writes
//! (0x0400_00A0 / 0x0400_00A4) and [`Apu::on_timer_overflow`].

/// Which FIFOs want a DMA refill after a timer overflow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FifoRequest {
    pub a: bool,
    pub b: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Apu {
    _reserved: (),
}

impl Apu {
    /// Timer `n` (0 or 1) overflowed: FIFOs clocked by it pop a sample, and any
    /// that fall to 16 bytes or fewer request a refill. **Contract.**
    pub fn on_timer_overflow(&mut self, n: usize) -> FifoRequest {
        let _ = n;
        FifoRequest::default()
    }

    /// A 32-bit write to FIFO A (`b = false`) or B. **Contract.**
    pub fn write_fifo(&mut self, b: bool, value: u32) {
        let _ = (b, value);
    }
}
