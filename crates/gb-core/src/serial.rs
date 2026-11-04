//! Serial port SB/SC (Pan Docs: "Serial Data Transfer (Link Cable)").
//!
//! No link partner is emulated: an internally clocked transfer shifts in 1 bits
//! (line pulled high) and completes after 8 shifts. Every byte the game sends
//! is captured; the test harness reads Blargg results from it.
//!
//! The 8192 Hz serial clock is the system counter divided down (bit 8 of the
//! 16-bit counter that also drives DIV), so a transfer's bit edges align with
//! the counter's phase at *reset*, not with the moment SC is written (mooneye
//! `boot_sclk_align`). The bus does not hand the timer's counter to this unit,
//! so it keeps its own copy, started at the post-boot value and advanced every
//! M-cycle. It therefore ignores DIV resets (a DIV write or STOP).
//!
//! To make it exact, `Bus::tick_components` would pass the timer's counter
//! (`timer.div_counter()`, ticked before serial) and the edge would be detected
//! on `counter.wrapping_add(4)`: the mirror below runs 4 T-cycles ahead of the
//! timer's counter, a calibration against `boot_sclk_align` with no hardware
//! explanation yet. Passing the raw counter would be one M-cycle late.

use crate::interrupts::{Interrupt, Interrupts};

/// Counter bit whose falling edge clocks the serial shifter (period 512 T).
const CLOCK_BIT: u16 = 1 << 8;
/// Shifts per transferred byte.
const BITS_PER_BYTE: u8 = 8;
/// Phase of this unit's counter copy at power-on. The timer's own post-boot
/// counter is 0xABC8; this one runs a further M-cycle ahead, which is what
/// mooneye `boot_sclk_align` needs (IRQ one M-cycle before the 8th edge of the
/// timer's counter). A calibration, not a derived hardware value.
const POST_BOOT_COUNTER: u16 = 0xABCC;
/// Cap captured output so a ROM spamming the port can't grow memory unboundedly.
const MAX_CAPTURE: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct Serial {
    sb: u8,
    sc: u8,
    /// Copy of the system counter (see the module docs).
    counter: u16,
    /// Shifts still to do; 0 when no transfer is running.
    bits_left: u8,
    output: Vec<u8>,
}

impl Default for Serial {
    fn default() -> Self {
        Self::new()
    }
}

impl Serial {
    pub fn new() -> Self {
        Self {
            sb: 0x00,
            sc: 0x7E,
            counter: POST_BOOT_COUNTER,
            bits_left: 0,
            output: Vec::new(),
        }
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF01 => self.sb,
            _ => self.sc | 0x7E,
        }
    }

    pub fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0xFF01 => self.sb = value,
            _ => {
                self.sc = value | 0x7E;
                if value & 0x81 == 0x81 {
                    if self.output.len() < MAX_CAPTURE {
                        self.output.push(self.sb);
                    }
                    self.bits_left = BITS_PER_BYTE;
                } else {
                    self.bits_left = 0;
                }
            }
        }
    }

    pub fn tick_mcycle(&mut self, interrupts: &mut Interrupts) {
        let before = self.counter;
        self.counter = self.counter.wrapping_add(4);
        let falling_edge = before & CLOCK_BIT != 0 && self.counter & CLOCK_BIT == 0;
        if self.bits_left > 0 && falling_edge {
            self.sb = self.sb << 1 | 1;
            self.bits_left -= 1;
            if self.bits_left == 0 {
                self.sc &= 0x7F;
                interrupts.request(Interrupt::Serial);
            }
        }
    }

    /// All bytes sent since power-on (or the last [`Serial::take_output`]).
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M-cycles from the start of a transfer until the interrupt, ticking from
    /// `ticks_before` M-cycles after power-on.
    fn mcycles_to_interrupt(ticks_before: u32) -> u32 {
        let mut s = Serial::new();
        let mut i = Interrupts::new();
        i.write_if(0);
        for _ in 0..ticks_before {
            s.tick_mcycle(&mut i);
        }
        s.write(0xFF01, b'P');
        s.write(0xFF02, 0x81);
        assert_eq!(s.output(), b"P");
        let mut n = 0;
        while i.read_if() & 0x08 == 0 {
            s.tick_mcycle(&mut i);
            n += 1;
            assert!(n <= 2 * 1024, "transfer never completed");
        }
        assert_eq!(s.read(0xFF02) & 0x80, 0);
        assert_eq!(s.read(0xFF01), 0xFF);
        n
    }

    #[test]
    fn completes_after_eight_clock_edges() {
        // The first edge comes at most 128 M-cycles after the start, the
        // 8th (the interrupt) 7 * 128 M-cycles later.
        let n = mcycles_to_interrupt(26);
        assert!((7 * 128 + 1..=8 * 128).contains(&n), "took {n}");
    }

    #[test]
    fn edges_align_to_the_counter_not_the_start() {
        // Starting one M-cycle later in the same 128-cycle window completes
        // one M-cycle sooner (mooneye boot_sclk_align).
        assert_eq!(mcycles_to_interrupt(26), 1037 - 26);
        assert_eq!(mcycles_to_interrupt(27), 1037 - 27);
        // Just after an edge, the next one is a full period away.
        assert_eq!(mcycles_to_interrupt(141), 7 * 128 + 128);
    }

    #[test]
    fn external_clock_never_completes() {
        let mut s = Serial::new();
        let mut i = Interrupts::new();
        i.write_if(0);
        s.write(0xFF02, 0x80);
        for _ in 0..4096 {
            s.tick_mcycle(&mut i);
        }
        assert_eq!(i.read_if() & 0x08, 0);
    }
}
