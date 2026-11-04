//! CGB clocking on the bus: the KEY1 speed switch, double speed and the HDMA
//! stall (Pan Docs "KEY1", "LCD VRAM DMA Transfers"; DECISIONS #38, #39).
//!
//! All of it is `Hw::Cgb` only. A DMG, and a CGB in compatibility mode, never
//! set [`Bus::slow`], so [`Bus::tick_components`] never leaves its plain path.
//!
//! # Clock domains
//!
//! One bus tick is one *CPU* M-cycle. In double speed the CPU, timer / DIV,
//! serial and OAM DMA run per CPU M-cycle (twice as fast as real time); the PPU,
//! the APU and the cartridge clock keep their real-time rate:
//!
//! * PPU: 2 dots per CPU M-cycle instead of 4 ([`Ppu::tick_mcycle_double`]).
//! * APU and cartridge RTC: ticked on every other CPU M-cycle (the even ones),
//!   so their per-M-cycle logic, the resampler and the sample count per real
//!   second are untouched. The frame sequencer is handed `counter >> 1`, i.e.
//!   system-counter bit 13 (DIV bit 5), which is the real-time DIV-APU rate.
//! * A frame is twice as many CPU M-cycles; `GameBoy::step_frame` allows for it.
//!
//! # Speed switch
//!
//! STOP with KEY1.0 set (and no selected joypad line low) spends the padding
//! byte's M-cycle, resets DIV, flips the speed, clears KEY1.0 and then pauses the
//! CPU for [`SPEED_SWITCH_MCYCLES`] M-cycles (of the new speed), all inside
//! `stop()`; the CPU then carries on after the STOP. The timer (DIV, TIMA), serial,
//! the PPU, the APU, OAM DMA and the cartridge keep running through the pause,
//! as on hardware (AGE `spsw-tima`: a 4 kHz TIMA counts 128 steps during it).
//! 32768 M-cycles is 2^17 system-counter steps, so DIV reads 0 again right after
//! the pause and counts on from there (AGE `spsw-div`, which no other length
//! passes, as `boot_div` did for the post-boot counter). A pending interrupt ends
//! the pause early, like a HALT (SameBoy). There is nothing to save: the pause
//! never spans a `step_instruction` boundary.
//!
//! The pause is a lot longer than Pan Docs' "2050 M-cycles"; the hardware
//! evidence above and SameBoy (0x20008 T-cycles) agree with each other
//! (DECISIONS #38).
//!
//! # HDMA
//!
//! The copy runs on the bus, which keeps ticking every component through the
//! stall, so the CPU just sees time pass. One *burst* costs one M-cycle of
//! start-up plus 8 M-cycles per 0x10-byte block at normal speed (2 bytes per
//! M-cycle) or 16 in double speed (1 byte per M-cycle): the same real time.
//! That is SameBoy's `GB_hdma_run`. A general-purpose DMA is one burst for the
//! whole length, started by the FF55 write. An HBlank DMA is one burst per block,
//! started on the first CPU bus tick after the PPU signalled HBlank entry.
//!
//! * HALT / STOP: the CPU's idle ticks do not start a block. The PPU's signal
//!   stays raised, so a block is copied on the first busy tick after waking if
//!   the PPU is still in HBlank by then; a signal from an earlier line is
//!   dropped. (Pan Docs: halting the CPU pauses the transfer.)
//! * LCD off: no HBlank edges happen, so no blocks run, but a transfer started
//!   with the LCD off gets one block at once, and so does an armed transfer when
//!   the LCD is switched off outside HBlank (SameBoy: the STAT mode reads 0).
//!   A general-purpose DMA is unaffected by the LCD.
//! * Starting an HBlank DMA while the PPU is in HBlank copies a block at once.
//! * Sources: ROM, cartridge RAM and WRAM (0000-7FFF, A000-DFFF) are read
//!   directly (no OAM DMA bus conflicts). VRAM and E000+ are open bus on
//!   hardware (Pan Docs: "garbage"); they read `0xFF` here. Writes go to the
//!   VBK-selected VRAM bank whatever the PPU mode.

use crate::bus::Bus;
use crate::hdma::{BLOCK_BYTES, Start};

/// CPU M-cycles (of the new speed) a speed switch pauses the CPU for, after the
/// DIV reset: 2^17 system-counter steps. Pan Docs says 2050 M-cycles; the AGE
/// `spsw-div` and `spsw-tima` ROMs (hardware-verified) and SameBoy's 0x20008
/// T-cycles say this (DECISIONS #38).
pub(crate) const SPEED_SWITCH_MCYCLES: u32 = 32768;

impl Bus {
    /// Recompute [`Bus::slow`] after the speed or the HDMA state changed.
    pub(crate) fn refresh_slow(&mut self) {
        self.slow = self.double_speed || self.hdma.hblank_active();
    }

    /// The CPU runs at double speed (KEY1 bit 7).
    pub fn double_speed(&self) -> bool {
        self.double_speed
    }

    /// KEY1 as read.
    pub(crate) fn read_key1(&self) -> u8 {
        0x7E | u8::from(self.double_speed) << 7 | u8::from(self.key1_prepare)
    }

    /// The slow path of [`Bus::tick_components`]: double speed and / or an HBlank
    /// DMA is running. `idle` is true for a halted or stopped CPU.
    #[cold]
    #[inline(never)]
    pub(crate) fn tick_slow(&mut self, idle: bool) {
        if !idle
            && self.hdma.hblank_active()
            && self.ppu.take_hblank_entered()
            && self.ppu.in_hdma_window()
        {
            self.hdma_burst(false);
        }
        self.tick_core();
    }

    /// One CPU M-cycle of the CGB clocking (see the module docs).
    pub(crate) fn tick_core(&mut self) {
        self.mcycles += 1;
        self.timer.tick_mcycle(&mut self.interrupts);
        self.serial
            .tick_mcycle(&mut self.interrupts, self.timer.div_counter());
        self.step_dma();
        if self.double_speed {
            self.ppu.tick_mcycle_double(&mut self.interrupts);
            if self.mcycles & 1 == 0 {
                self.apu.tick_mcycle(self.timer.div_counter() >> 1);
                if self.cart_ticks {
                    self.cart.tick_mcycle();
                }
            }
        } else {
            self.ppu.tick_mcycle(&mut self.interrupts);
            self.apu.tick_mcycle(self.timer.div_counter());
            if self.cart_ticks {
                self.cart.tick_mcycle();
            }
        }
    }

    /// STOP with a speed switch armed (see the module docs).
    pub(crate) fn speed_switch(&mut self) {
        // The padding byte after STOP: the CPU model skips it without a bus
        // cycle, but on hardware the DIV reset lands after it (AGE `spsw-tima`).
        self.tick_components();
        self.timer.reset_div(&mut self.interrupts);
        self.key1_prepare = false;
        self.double_speed = !self.double_speed;
        self.ppu.set_double_speed(self.double_speed);
        let counter = self.timer.div_counter();
        self.apu.sync_sequencer(if self.double_speed {
            counter >> 1
        } else {
            counter
        });
        self.refresh_slow();
        // An interrupt that is (or becomes) pending ends the pause, like a HALT.
        for _ in 0..SPEED_SWITCH_MCYCLES {
            if self.interrupts.pending() != 0 {
                break;
            }
            self.tick_core();
        }
    }

    /// CPU write to FF55.
    pub(crate) fn write_hdma5(&mut self, value: u8) {
        match self.hdma.write5(value) {
            Start::Cancelled => {}
            Start::General => self.hdma_burst(true),
            Start::HBlank => {
                // A signal from an earlier line must not fire the new transfer;
                // starting inside HBlank (or with the LCD off) copies a block now.
                let now = self.ppu.in_hdma_window();
                self.ppu.set_hblank_entered(now);
            }
        }
        self.refresh_slow();
    }

    /// One burst: the start-up M-cycle, then the whole length (`general`) or a
    /// single block. Ticks every component while the CPU waits.
    fn hdma_burst(&mut self, general: bool) {
        self.tick_core();
        loop {
            let (src, dst) = self.hdma.next_block();
            self.hdma_copy_block(src, dst);
            if !general || self.hdma.general_done() {
                break;
            }
        }
        self.refresh_slow();
    }

    /// Copy one 0x10-byte block, spending 8 M-cycles at normal speed (two bytes
    /// per M-cycle) or 16 in double speed (one byte per M-cycle).
    fn hdma_copy_block(&mut self, src: u16, dst: u16) {
        let per_tick = if self.double_speed { 1 } else { 2 };
        for i in 0..BLOCK_BYTES {
            let byte = self.hdma_source_byte(src.wrapping_add(i));
            self.ppu.hdma_write_vram(dst + i, byte);
            if (i + 1) % per_tick == 0 {
                self.tick_core();
            }
        }
    }

    /// A source byte: ROM, cartridge RAM and WRAM; open bus (`0xFF`) elsewhere.
    fn hdma_source_byte(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7FFF => self.cart.read_rom(addr),
            0xA000..=0xBFFF => self.cart.read_ram(addr),
            0xC000..=0xDFFF => self.wram[self.wram_index(addr)],
            _ => 0xFF,
        }
    }
}

#[cfg(test)]
#[path = "bus_cgb_tests.rs"]
mod tests;
