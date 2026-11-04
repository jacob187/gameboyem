//! Serial port SB/SC (Pan Docs: "Serial Data Transfer (Link Cable)").
//!
//! No link partner is emulated: an internally clocked transfer shifts in 0xFF
//! (line pulled high) and completes after 8 bits x 128 M-cycles. Every byte
//! the game sends is captured; the test harness reads Blargg results from it.

use crate::interrupts::{Interrupt, Interrupts};

/// M-cycles per transferred byte at the 8192 Hz internal clock.
const TRANSFER_MCYCLES: u16 = 8 * 128;
/// Cap captured output so a ROM spamming the port can't grow memory unboundedly.
const MAX_CAPTURE: usize = 64 * 1024;

#[derive(Clone, Debug, Default)]
pub struct Serial {
    sb: u8,
    sc: u8,
    remaining: u16,
    output: Vec<u8>,
}

impl Serial {
    pub fn new() -> Self {
        Self {
            sb: 0x00,
            sc: 0x7E,
            remaining: 0,
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
                    self.remaining = TRANSFER_MCYCLES;
                }
            }
        }
    }

    pub fn tick_mcycle(&mut self, interrupts: &mut Interrupts) {
        if self.remaining > 0 {
            self.remaining -= 1;
            if self.remaining == 0 {
                self.sb = 0xFF;
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

    #[test]
    fn captures_byte_and_completes_transfer() {
        let mut s = Serial::new();
        let mut i = Interrupts::new();
        i.write_if(0);
        s.write(0xFF01, b'P');
        s.write(0xFF02, 0x81);
        assert_eq!(s.output(), b"P");
        for _ in 0..TRANSFER_MCYCLES {
            s.tick_mcycle(&mut i);
        }
        assert_eq!(s.read(0xFF02) & 0x80, 0);
        assert_eq!(i.read_if() & 0x08, 0x08);
        assert_eq!(s.read(0xFF01), 0xFF);
    }
}
