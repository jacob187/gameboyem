//! I/O register dispatch (0x0400_0000..0x0400_0400). Owned by P1-SYS.
//!
//! Registers are 16-bit with a byte-lane write mask; 8-bit accesses use one lane
//! and 32-bit accesses are two halfword accesses, low half first (so a 32-bit
//! write of DMA count + control, or timer reload + control, applies the count/
//! reload before the enable bit; GBATEK "GBA Timers"). Unused registers read as
//! open bus; write-only registers read as 0 (DMA SAD/DAD/CNT_L, FIFOs).

use crate::bus::{Bus, Width};
use crate::irq::Interrupt;
use crate::scheduler::Event;

/// WAITCNT's writable bits (bit 15, the cart type, is read-only).
const WAITCNT_MASK: u16 = 0x7FFF;

#[inline]
fn merge(old: u16, v: u16, mask: u16) -> u16 {
    (old & !mask) | (v & mask)
}

impl Bus {
    /// Read `w` at an I/O address; anything past 0x3FF is unmapped.
    pub(crate) fn read_io_value(&self, addr: u32, w: Width) -> u32 {
        let off = addr & 0x00FF_FFFF;
        if off >= 0x400 {
            return self.open_bus_value(addr, w);
        }
        match w {
            Width::B => u32::from(self.read_io16(off & !1) >> ((off & 1) * 8)) & 0xFF,
            Width::H => u32::from(self.read_io16(off & !1)),
            Width::W => {
                let o = off & !3;
                u32::from(self.read_io16(o)) | u32::from(self.read_io16(o + 2)) << 16
            }
        }
    }

    /// Write `w` at an I/O address.
    pub(crate) fn write_io_value(&mut self, addr: u32, v: u32, w: Width) {
        let off = addr & 0x00FF_FFFF;
        if off >= 0x400 {
            return;
        }
        match w {
            Width::B => {
                let sh = (off & 1) * 8;
                self.write_io16(off & !1, (v as u16 & 0xFF) << sh, 0xFF << sh);
            }
            Width::H => self.write_io16(off & !1, v as u16, 0xFFFF),
            Width::W => {
                let o = off & !3;
                self.write_io16(o, v as u16, 0xFFFF);
                self.write_io16(o + 2, (v >> 16) as u16, 0xFFFF);
            }
        }
    }

    /// I/O read at the even offset `off` (0..0x3FF from 0x0400_0000).
    pub fn read_io16(&self, off: u32) -> u16 {
        match off {
            0x000..=0x05F => self.ppu.read_io(off),
            // P2-APU: the sound registers live in the APU (it applies the read masks).
            0x060..=0x09F => self.apu.read_reg(off),
            0x0A0..=0x0A7 => 0, // FIFOs are write-only
            0x0B0..=0x0DF => self.dma.read_reg(off - 0xB0),
            0x100..=0x10F => {
                let n = ((off - 0x100) >> 2) as usize;
                if off & 2 == 0 {
                    self.timers.read_counter(n, self.sched.now())
                } else {
                    self.timers.read_control(n)
                }
            }
            0x130 => self.keypad.keyinput(),
            0x132 => self.keypad.keycnt,
            0x120..=0x12F | 0x134..=0x15F => self.sio_regs[(off as usize - 0x120) >> 1],
            0x200 => self.irq.ie,
            0x202 => self.irq.if_,
            0x204 => self.waitcnt,
            0x208 => u16::from(self.irq.ime),
            0x206 | 0x20A => 0,
            0x300 => u16::from(self.postflg),
            _ => (self.open_bus >> ((off & 2) * 8)) as u16,
        }
    }

    /// I/O write at the even offset `off`; `mask` selects the byte lanes written.
    pub fn write_io16(&mut self, off: u32, v: u16, mask: u16) {
        match off {
            0x000..=0x05F => self.ppu.write_io(off, v, mask),
            // P2-APU: sync the APU to the clock, then write.
            0x060..=0x09F => {
                self.apu.advance_to(self.sched.now());
                self.apu.write_reg(off, v, mask);
            }
            0x0A0..=0x0A7 => self.write_fifo_half(off, v, mask),
            0x0B0..=0x0DF => self.dma.write_reg(off - 0xB0, v, mask),
            0x100..=0x10F => {
                let n = ((off - 0x100) >> 2) as usize;
                if off & 2 == 0 {
                    self.timers.write_reload(n, v, mask);
                } else {
                    let now = self.sched.now();
                    self.timers.write_control(n, v, mask, now, &mut self.sched);
                }
            }
            0x132 => {
                self.keypad.write_keycnt(v, mask);
                self.keypad_irq();
            }
            0x120..=0x12F | 0x134..=0x15F => {
                let r = &mut self.sio_regs[(off as usize - 0x120) >> 1];
                *r = merge(*r, v, mask);
            }
            0x200 => self.irq.ie = merge(self.irq.ie, v, mask) & 0x3FFF,
            0x202 => self.irq.write_if(v & mask),
            0x204 => {
                self.waitcnt = merge(self.waitcnt, v, mask) & WAITCNT_MASK;
                self.waits = crate::bus::WaitTable::from_waitcnt(self.waitcnt);
            }
            0x208 => {
                if mask & 1 != 0 {
                    self.irq.ime = v & 1 != 0;
                }
            }
            0x300 => {
                if mask & 0x00FF != 0 {
                    self.postflg = (v & 1) as u8;
                }
                // HALTCNT is the high byte: bit 7 selects stop over halt.
                if mask & 0xFF00 != 0 {
                    self.halted = true;
                    self.stopped = v & 0x8000 != 0;
                    // An enabled interrupt that is already pending ends the halt
                    // at once, without a clock skip (code-review S#3).
                    self.check_wake();
                }
            }
            _ => {}
        }
    }

    /// A halfword (or byte lane) of FIFO A/B. The word is pushed to the APU when
    /// its high half is written, so a 32-bit write (low half first) pushes once.
    fn write_fifo_half(&mut self, off: u32, v: u16, mask: u16) {
        let b = usize::from(off >= 0xA4);
        let hi = off & 2 != 0;
        let shift = if hi { 16 } else { 0 };
        let m = u32::from(mask) << shift;
        self.fifo_latch[b] = (self.fifo_latch[b] & !m) | (u32::from(v) << shift & m);
        if hi && mask & 0xFF00 != 0 {
            self.apu.write_fifo(b == 1, self.fifo_latch[b]);
        }
    }

    /// Re-check the KEYCNT interrupt condition and raise IF.Keypad on a new edge.
    pub fn keypad_irq(&mut self) {
        if self.keypad.irq_edge() {
            self.irq.raise(Interrupt::Keypad);
            self.check_wake();
        }
    }

    /// Press or release a button.
    pub fn set_button(&mut self, b: crate::keypad::Button, pressed: bool) {
        self.keypad.set(b, pressed);
        self.keypad_irq();
    }

    /// Timer `n` overflowed at `at`: reload, IRQ, APU clocking, FIFO DMA and the
    /// cascade into the next timer.
    pub(crate) fn timer_overflow(&mut self, n: usize, at: u64) {
        let mut n = n;
        loop {
            if self.timers.overflow(n, at, &mut self.sched) {
                self.irq.raise(Interrupt::timer(n));
            }
            if n < 2 {
                // P2-APU: the FIFO pop lands at the overflow's own time.
                self.apu.advance_to(at);
                let req = self.apu.on_timer_overflow(n);
                if req.a {
                    self.dma.trigger(crate::dma::DmaTrigger::FifoA);
                }
                if req.b {
                    self.dma.trigger(crate::dma::DmaTrigger::FifoB);
                }
            }
            if n < 3 && self.timers.cascade(n + 1) {
                n += 1;
                // The cascaded timer overflowed too: handle it at the same instant
                // (it is count-up, so it has no event of its own to cancel).
                self.sched.cancel(Event::TimerOverflow(n as u8));
                continue;
            }
            return;
        }
    }
}
