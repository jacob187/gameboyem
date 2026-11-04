//! Serial port SB/SC (Pan Docs: "Serial Data Transfer (Link Cable)").
//!
//! No link partner is emulated: an internally clocked transfer shifts in 1 bits
//! (line pulled high) and completes after 8 shifts. Every byte the game sends
//! is captured when its transfer completes; the test harness reads Blargg
//! results from it.
//!
//! The 8192 Hz serial clock is the system counter divided down (bit 8 of the
//! 16-bit counter that also drives DIV), so a transfer's bit edges align with
//! the counter's phase at *reset*, not with the moment SC is written (mooneye
//! `boot_sclk_align`). The bus hands this unit the timer's counter every
//! M-cycle (`Timer::div_counter()`, ticked before serial), so DIV writes and
//! STOP re-phase the serial clock exactly as they re-phase DIV.
//!
//! The falling edge is detected on `counter + 4`: the clock edge reaches the
//! shifter one M-cycle before the raw counter shows it. This offset is a
//! calibration against `boot_sclk_align` with no hardware explanation yet;
//! passing the raw counter would be one M-cycle late.
//!
//! Edge detection is stateless (the counter advances by 4 per M-cycle, so the
//! edge is the step `counter -> counter + 4`), which lets an idle port return
//! immediately. A DIV reset in the middle of a transfer therefore never
//! produces an extra shift; it only moves the following edges.
//!
//! Restarting (writing SC with start and internal clock) in the middle of a
//! transfer reloads the 8-shift count; SB keeps its partially shifted contents,
//! which become the byte the new transfer sends. The aborted transfer's byte is
//! never captured, because it never finished.

use crate::interrupts::{Interrupt, Interrupts};

/// Counter bit whose falling edge clocks the serial shifter (period 512 T).
const CLOCK_BIT: u16 = 1 << 8;
/// Shifts per transferred byte.
const BITS_PER_BYTE: u8 = 8;
/// Cap captured output so a ROM spamming the port can't grow memory unboundedly.
const MAX_CAPTURE: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct Serial {
    sb: u8,
    sc: u8,
    /// Shifts still to do; 0 when no transfer is running.
    bits_left: u8,
    /// The byte the running transfer is sending (SB at the moment it started).
    tx: u8,
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
            bits_left: 0,
            tx: 0,
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
                    self.tx = self.sb;
                    self.bits_left = BITS_PER_BYTE;
                } else {
                    self.bits_left = 0;
                }
            }
        }
    }

    /// Advance one M-cycle. `counter` is the system counter after this
    /// M-cycle's timer tick (see the module docs).
    #[inline]
    pub fn tick_mcycle(&mut self, interrupts: &mut Interrupts, counter: u16) {
        if self.bits_left == 0 {
            return;
        }
        let falling_edge = counter & CLOCK_BIT != 0 && counter.wrapping_add(4) & CLOCK_BIT == 0;
        if falling_edge {
            self.shift(interrupts);
        }
    }

    fn shift(&mut self, interrupts: &mut Interrupts) {
        self.sb = self.sb << 1 | 1;
        self.bits_left -= 1;
        if self.bits_left == 0 {
            self.sc &= 0x7F;
            if self.output.len() < MAX_CAPTURE {
                self.output.push(self.tx);
            }
            interrupts.request(Interrupt::Serial);
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

    /// A stand-in for the timer: the system counter, post-boot, advancing 4
    /// per M-cycle and ticked before the serial port, as in `Bus`.
    struct Rig {
        s: Serial,
        i: Interrupts,
        counter: u16,
    }

    impl Rig {
        fn new() -> Self {
            let mut i = Interrupts::new();
            i.write_if(0);
            Self {
                s: Serial::new(),
                i,
                counter: 0xABC8,
            }
        }

        fn tick(&mut self) {
            self.counter = self.counter.wrapping_add(4);
            self.s.tick_mcycle(&mut self.i, self.counter);
        }

        fn ticks(&mut self, n: u32) {
            for _ in 0..n {
                self.tick();
            }
        }

        fn start(&mut self, byte: u8) {
            self.s.write(0xFF01, byte);
            self.s.write(0xFF02, 0x81);
        }

        fn irq(&self) -> bool {
            self.i.read_if() & 0x08 != 0
        }

        /// M-cycles until the serial interrupt.
        fn run_to_interrupt(&mut self) -> u32 {
            let mut n = 0;
            while !self.irq() {
                self.tick();
                n += 1;
                assert!(n <= 2 * 1024, "transfer never completed");
            }
            n
        }
    }

    /// M-cycles from the start of a transfer until the interrupt, ticking from
    /// `ticks_before` M-cycles after power-on.
    fn mcycles_to_interrupt(ticks_before: u32) -> u32 {
        let mut r = Rig::new();
        r.ticks(ticks_before);
        r.start(b'P');
        assert!(r.s.output().is_empty(), "captured only on completion");
        let n = r.run_to_interrupt();
        assert_eq!(r.s.output(), b"P");
        assert_eq!(r.s.read(0xFF02) & 0x80, 0);
        assert_eq!(r.s.read(0xFF01), 0xFF);
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
    fn delay_after_a_div_reset_does_not_depend_on_the_earlier_phase() {
        // A DIV write (or STOP) zeroes the system counter, which re-phases the
        // serial clock. Whatever the phase was before the reset, a transfer
        // started a fixed time after it takes the same time.
        let after_reset = |ticks_before_reset: u32, ticks_after_reset: u32| {
            let mut r = Rig::new();
            r.ticks(ticks_before_reset);
            r.counter = 0;
            r.ticks(ticks_after_reset);
            r.start(0x42);
            r.run_to_interrupt()
        };
        let reference = after_reset(0, 5);
        for before in [1, 2, 3, 7, 31, 64, 127, 128, 129, 200, 255, 256, 511, 1000] {
            assert_eq!(after_reset(before, 5), reference, "reset after {before}");
        }
        // And it is the counter's phase, not the start, that sets the delay.
        assert_eq!(after_reset(9, 6), reference - 1);
    }

    #[test]
    fn external_clock_never_completes() {
        let mut r = Rig::new();
        r.s.write(0xFF02, 0x80);
        r.ticks(4096);
        assert!(!r.irq());
        assert!(r.s.output().is_empty());
    }

    #[test]
    fn restart_mid_transfer_reloads_the_count_and_sends_the_partial_byte() {
        let mut r = Rig::new();
        r.start(b'P'); // 0x50
        // Run until exactly 3 bits have shifted.
        while r.s.bits_left > BITS_PER_BYTE - 3 {
            r.tick();
        }
        assert_eq!(r.s.read(0xFF01), 0x87);
        r.s.write(0xFF02, 0x81);
        assert!(
            r.s.output().is_empty(),
            "the aborted byte is never captured"
        );
        assert_eq!(r.s.read(0xFF02) & 0x80, 0x80);
        // A full 8 shifts are needed again, and the byte sent is what SB held.
        assert_eq!(r.s.bits_left, BITS_PER_BYTE);
        let mut shifts = 0;
        let mut left = r.s.bits_left;
        while !r.irq() {
            r.tick();
            if r.s.bits_left != left {
                shifts += 1;
                left = r.s.bits_left;
            }
        }
        assert_eq!(shifts, 8);
        assert_eq!(r.s.output(), [0x87]);
        assert_eq!(r.s.read(0xFF01), 0xFF);
    }

    #[test]
    fn clearing_start_mid_transfer_aborts_without_capture() {
        let mut r = Rig::new();
        r.start(b'P');
        r.ticks(300);
        r.s.write(0xFF02, 0x01);
        r.ticks(2048);
        assert!(!r.irq());
        assert!(r.s.output().is_empty());
    }
}
