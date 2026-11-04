//! DIV/TIMA/TMA/TAC timer (Pan Docs: "Timer and Divider Registers",
//! "Timer obscure behaviour").
//!
//! Modelled as a 16-bit system counter incremented every T-cycle; DIV is its
//! upper byte. TIMA increments on the *falling edge* of
//! `counter[bit(TAC)] & TAC.enable`, which naturally reproduces the DIV-write
//! and TAC-write glitches. On overflow TIMA reads 0 for one M-cycle, then is
//! reloaded from TMA and the timer interrupt is requested.
//!
//! # M-cycle ordering (see `bus.rs`)
//!
//! Every CPU access is `tick_mcycle` first, then the register access. Using
//! `T` for the tick in which the falling edge overflows TIMA (verified against
//! mooneye `tima_reload`, `tima_write_reloading`, `tma_write_reloading`):
//!
//! | access in tick | TIMA reads | TIMA write         | TMA write                |
//! |----------------|------------|--------------------|--------------------------|
//! | `T`            | `0x00`     | cancels reload+IRQ | picked up by the reload  |
//! | `T + 1`        | `TMA`      | ignored            | copied into TIMA too     |
//! | `T + 2`        | normal     | normal             | too late for TIMA        |
//!
//! The `T + 1` row is Pan Docs' reload cycle where "TMA writes propagate";
//! mooneye `tma_write_reloading` requires it (a write there reads back in TIMA).

use crate::interrupts::{Interrupt, Interrupts};

/// System counter when the first cartridge instruction executes after the DMG
/// boot ROM (DIV = 0xAB). The low byte is the value that makes mooneye
/// `boot_div-dmgABCmgb` pass (found by scanning every M-cycle phase).
const POST_BOOT_COUNTER: u16 = 0xABC8;

#[derive(Clone, Debug)]
pub struct Timer {
    counter: u16,
    tima: u8,
    tma: u8,
    tac: u8,
    /// TIMA overflowed during the previous M-cycle; reload happens this cycle.
    overflow_pending: bool,
    /// TIMA was reloaded from TMA during the current M-cycle; CPU writes to TIMA
    /// in this cycle are ignored and TMA writes are copied into TIMA.
    reloaded_this_cycle: bool,
}

impl Default for Timer {
    fn default() -> Self {
        Self::new()
    }
}

impl Timer {
    pub fn new() -> Self {
        Self {
            counter: POST_BOOT_COUNTER,
            tima: 0,
            tma: 0,
            tac: 0xF8,
            overflow_pending: false,
            reloaded_this_cycle: false,
        }
    }

    /// The full 16-bit system counter (DIV is the upper byte). The APU frame
    /// sequencer clocks off its bit 12 (DIV bit 4).
    pub fn div_counter(&self) -> u16 {
        self.counter
    }

    fn selected_bit(&self) -> u16 {
        match self.tac & 0x03 {
            0 => 1 << 9,
            1 => 1 << 3,
            2 => 1 << 5,
            _ => 1 << 7,
        }
    }

    fn timer_signal(&self) -> bool {
        self.tac & 0x04 != 0 && self.counter & self.selected_bit() != 0
    }

    fn increment_tima(&mut self) {
        let (v, overflow) = self.tima.overflowing_add(1);
        self.tima = v;
        if overflow {
            self.overflow_pending = true;
        }
    }

    /// Advance one M-cycle (4 T-cycles).
    pub fn tick_mcycle(&mut self, interrupts: &mut Interrupts) {
        self.reloaded_this_cycle = false;
        if self.overflow_pending {
            self.overflow_pending = false;
            self.tima = self.tma;
            self.reloaded_this_cycle = true;
            interrupts.request(Interrupt::Timer);
        }
        let before = self.timer_signal();
        self.counter = self.counter.wrapping_add(4);
        if before && !self.timer_signal() {
            self.increment_tima();
        }
    }

    /// Reset the system counter (DIV write or STOP). May clock TIMA.
    pub fn reset_div(&mut self, _interrupts: &mut Interrupts) {
        let before = self.timer_signal();
        self.counter = 0;
        if before {
            self.increment_tima();
        }
    }

    pub fn read(&self, addr: u16) -> u8 {
        match addr {
            0xFF04 => (self.counter >> 8) as u8,
            0xFF05 => self.tima,
            0xFF06 => self.tma,
            0xFF07 => self.tac | 0xF8,
            _ => 0xFF,
        }
    }

    pub fn write(&mut self, addr: u16, value: u8, interrupts: &mut Interrupts) {
        match addr {
            0xFF04 => self.reset_div(interrupts),
            0xFF05 => {
                if !self.reloaded_this_cycle {
                    self.tima = value;
                    // Writing TIMA during the overflow cycle cancels the reload.
                    self.overflow_pending = false;
                }
            }
            0xFF06 => {
                self.tma = value;
                if self.reloaded_this_cycle {
                    self.tima = value;
                }
            }
            0xFF07 => {
                let before = self.timer_signal();
                self.tac = value | 0xF8;
                if before && !self.timer_signal() {
                    self.increment_tima();
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timer_at_zero() -> (Timer, Interrupts) {
        let mut t = Timer::new();
        let mut i = Interrupts::new();
        i.write_if(0);
        t.write(0xFF04, 0, &mut i);
        (t, i)
    }

    #[test]
    fn div_increments_every_64_mcycles() {
        let (mut t, mut i) = timer_at_zero();
        for _ in 0..63 {
            t.tick_mcycle(&mut i);
        }
        assert_eq!(t.read(0xFF04), 0);
        t.tick_mcycle(&mut i);
        assert_eq!(t.read(0xFF04), 1);
    }

    #[test]
    fn tima_at_262khz_overflows_and_reloads() {
        let (mut t, mut i) = timer_at_zero();
        t.write(0xFF06, 0xF0, &mut i);
        t.write(0xFF05, 0xFF, &mut i);
        t.write(0xFF07, 0x05, &mut i); // enable, 16 T-cycles
        for _ in 0..4 {
            t.tick_mcycle(&mut i);
        }
        assert_eq!(t.read(0xFF05), 0x00, "TIMA reads 0 during overflow cycle");
        assert_eq!(i.read_if() & 0x04, 0);
        t.tick_mcycle(&mut i);
        assert_eq!(t.read(0xFF05), 0xF0);
        assert_eq!(i.read_if() & 0x04, 0x04);
    }

    #[test]
    fn div_write_can_clock_tima() {
        let (mut t, mut i) = timer_at_zero();
        t.write(0xFF07, 0x05, &mut i);
        for _ in 0..2 {
            t.tick_mcycle(&mut i); // counter = 8, bit 3 set
        }
        t.write(0xFF04, 0, &mut i);
        assert_eq!(t.read(0xFF05), 1);
    }
}
