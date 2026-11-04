//! The system bus: the memory interface the CPU codes against ([`CpuBus`]) and
//! the concrete [`Bus`] that owns every other component.
//!
//! # Contract (frozen; propose changes to the orchestrator)
//!
//! * Every [`CpuBus::read`] / [`CpuBus::write`] costs exactly **one M-cycle**
//!   (4 T-cycles), and the rest of the machine is advanced by that M-cycle
//!   *inside* the call.
//! * Instruction cycles that do not touch memory (e.g. the extra cycle of
//!   `INC rr`, `PUSH`, taken branches, interrupt dispatch) are spent with
//!   [`CpuBus::tick`].
//! * Therefore an instruction's total cycle count is simply the number of
//!   `read` + `write` + `tick` calls it makes. Opcode fetches are reads.
//! * [`CpuBus::pending_interrupts`] and [`CpuBus::acknowledge_interrupt`] are
//!   free (no cycle) and are how the CPU observes and services IE/IF.

use crate::apu::Apu;
use crate::cartridge::{Cartridge, MbcKind};
use crate::dma::Dma;
use crate::hdma::Hdma;
use crate::interrupts::Interrupts;
use crate::joypad::Joypad;
use crate::model::{Hw, Model, compat_b};
use crate::ppu::Ppu;
use crate::serial::Serial;
use crate::state::{StateError, StateReader, StateWriter, check, tag};
use crate::timer::Timer;

/// The CPU's view of the machine. Implemented by [`Bus`] and by flat-memory
/// test buses in CPU unit tests.
pub trait CpuBus {
    /// Read a byte, advancing the machine by one M-cycle.
    fn read(&mut self, addr: u16) -> u8;
    /// Write a byte, advancing the machine by one M-cycle.
    fn write(&mut self, addr: u16, value: u8);
    /// Spend one internal M-cycle with no memory access.
    fn tick(&mut self);
    /// `IE & IF & 0x1F`, without spending a cycle.
    fn pending_interrupts(&self) -> u8;
    /// Clear the given IF bit(s), without spending a cycle.
    fn acknowledge_interrupt(&mut self, mask: u8);
    /// Called when the CPU executes `STOP` (DMG: resets DIV). No cycle cost,
    /// except that a CGB speed switch spends its whole pause inside this call.
    /// Returns true if STOP performed a speed switch, in which case the CPU
    /// carries on instead of entering STOP.
    fn stop(&mut self) -> bool {
        false
    }
    /// Spend one M-cycle while the CPU is halted or stopped (or locked up). The
    /// default is an ordinary [`CpuBus::tick`]; the real bus uses it to tell an
    /// idle CPU from a busy one, because HBlank DMA only runs for a busy CPU.
    fn idle_tick(&mut self) {
        self.tick();
    }
    /// True while any *selected* P1 input line is low (button held). This is
    /// the STOP wake-up condition and is independent of IE. No cycle cost.
    fn joypad_line_low(&self) -> bool {
        false
    }
}

/// Bytes of WRAM a DMG has (two 4 KiB banks' worth).
pub(crate) const WRAM_SIZE: usize = 0x2000;
/// Bytes of WRAM a CGB has: 8 banks of 4 KiB.
pub(crate) const CGB_WRAM_SIZE: usize = 0x8000;
/// Offset of the switchable window (D000-DFFF) in `wram` while bank 1 is selected,
/// which is where a DMG's second 4 KiB lives: the DMG layout is the CGB one with
/// bank 1 selected.
const WRAM_BANK1: usize = 0x1000;
pub(crate) const HRAM_SIZE: usize = 0x7F;

/// Owns all non-CPU components and routes memory accesses (see `mmu.rs`).
pub struct Bus {
    pub(crate) cart: Box<dyn Cartridge>,
    /// All 8 CGB banks of 4 KiB (bank 0 first); a DMG uses the first 8 KiB.
    pub(crate) wram: Box<[u8; CGB_WRAM_SIZE]>,
    pub(crate) hram: [u8; HRAM_SIZE],
    pub(crate) interrupts: Interrupts,
    pub(crate) timer: Timer,
    pub(crate) ppu: Ppu,
    pub(crate) dma: Dma,
    pub(crate) apu: Apu,
    pub(crate) joypad: Joypad,
    pub(crate) serial: Serial,
    /// Total M-cycles elapsed since power-on.
    pub(crate) mcycles: u64,
    /// Derived (never saved): `double_speed || hdma.hblank_active()`. When false
    /// `tick_components` takes its plain path, the only one a DMG or a machine in
    /// compatibility mode ever runs. Set again by [`Bus::refresh_slow`].
    pub(crate) slow: bool,
    /// Whether the cartridge needs a per-M-cycle tick (only an MBC3 RTC does).
    pub(crate) cart_ticks: bool,
    // ---- CGB additions (DECISIONS #31, #33); VBK lives in the PPU ----
    /// The machine variant; only the IO paths in `mmu.rs` look at it.
    pub(crate) hw: Hw,
    /// SVBK (FF70) bits 0-2 as written (0 selects bank 1).
    pub(crate) svbk: u8,
    /// Offset in `wram` of the D000-DFFF window; precomputed from `svbk` so
    /// WRAM accesses don't branch on the model.
    pub(crate) wram_hi: usize,
    /// KEY1 (FF4D) bit 0: a speed switch is armed for the next STOP.
    pub(crate) key1_prepare: bool,
    /// KEY1 bit 7: the CPU runs at double speed (`Hw::Cgb` only; see
    /// `bus_cgb.rs`). Timer, serial and OAM DMA follow the CPU; the PPU, APU and
    /// cartridge clock keep their real-time rate.
    pub(crate) double_speed: bool,
    /// HDMA1-HDMA5 (FF51-FF55), `Hw::Cgb` only.
    pub(crate) hdma: Hdma,
    /// The undocumented read/write registers FF72-FF75 (CGB hardware only).
    pub(crate) cgb_scratch: CgbScratch,
}

/// FF72-FF75, the CGB's undocumented plain read/write registers (Pan Docs
/// "Undocumented registers"), stored as the machine holds them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CgbScratch {
    pub(crate) ff72: u8,
    pub(crate) ff73: u8,
    /// Mapped in CGB mode only; stays 0 in compatibility mode.
    pub(crate) ff74: u8,
    /// Only bits 4-6 are stored (the rest read 1): see [`CgbScratch::FF75_MASK`].
    pub(crate) ff75: u8,
}

impl CgbScratch {
    pub(crate) const FF75_MASK: u8 = 0x70;

    /// Written in the order FF72, FF73, FF75, FF74.
    fn save_state(&self, w: &mut StateWriter) {
        for v in [self.ff72, self.ff73, self.ff75, self.ff74] {
            w.u8(v);
        }
    }

    /// Reads what [`CgbScratch::save_state`] wrote. `ff74_mapped` is whether
    /// this machine maps FF74 (CGB mode); otherwise it can only be 0.
    fn load_state(r: &mut StateReader, ff74_mapped: bool) -> Result<Self, StateError> {
        let ff72 = r.u8()?;
        let ff73 = r.u8()?;
        let ff75 = r.u8()?;
        check(ff75 & !Self::FF75_MASK == 0, "FF75")?;
        let ff74 = r.u8()?;
        check(ff74_mapped || ff74 == 0, "FF74")?;
        Ok(Self {
            ff72,
            ff73,
            ff74,
            ff75,
        })
    }
}

impl Bus {
    pub fn new(cart: Box<dyn Cartridge>) -> Self {
        Self::with_model(cart, Model::Dmg)
    }

    /// Power on as `model`. On a CGB the boot ROM's choice between CGB mode and
    /// DMG-compatibility mode follows the cartridge header (see [`Hw`]).
    pub fn with_model(cart: Box<dyn Cartridge>, model: Model) -> Self {
        let header = cart.header();
        let cart_ticks = header.mbc == MbcKind::Mbc3 && header.has_rtc;
        let hw = Hw::new(model, header);
        let cgb = hw.is_cgb();
        Self {
            cart,
            wram: Box::new([0; CGB_WRAM_SIZE]),
            hram: [0; HRAM_SIZE],
            interrupts: Interrupts::new(),
            timer: if cgb { Timer::new_cgb() } else { Timer::new() },
            ppu: if cgb {
                Ppu::new_cgb(hw == Hw::Cgb)
            } else {
                Ppu::new()
            },
            dma: if cgb { Dma::new_cgb() } else { Dma::new() },
            apu: Apu::new(),
            joypad: if cgb {
                Joypad::new_cgb(hw == Hw::CgbCompat)
            } else {
                Joypad::new()
            },
            serial: if hw == Hw::Cgb {
                Serial::new_cgb_mode()
            } else {
                Serial::new()
            },
            mcycles: 0,
            cart_ticks,
            hw,
            svbk: 0,
            wram_hi: WRAM_BANK1,
            key1_prepare: false,
            double_speed: false,
            hdma: Hdma::new(),
            slow: false,
            cgb_scratch: CgbScratch::default(),
        }
    }

    /// The registers the CPU starts with: the model's post-boot hand-off state.
    pub(crate) fn post_boot_registers(&self) -> crate::cpu::Registers {
        use crate::cpu::Registers;
        match self.hw {
            Hw::Dmg => Registers::dmg_post_boot(),
            Hw::CgbCompat => Registers::cgb_compat_post_boot(compat_b(&*self.cart)),
            Hw::Cgb => Registers::cgb_post_boot(),
        }
    }

    /// Select the D000-DFFF WRAM bank from an SVBK write (bank 0 maps to 1).
    pub(crate) fn set_svbk(&mut self, value: u8) {
        self.svbk = value & 7;
        self.wram_hi = usize::from(self.svbk.max(1)) * WRAM_BANK1;
    }

    /// Bytes of `wram` this machine has.
    fn wram_len(&self) -> usize {
        if self.hw.is_cgb() {
            CGB_WRAM_SIZE
        } else {
            WRAM_SIZE
        }
    }

    /// Total M-cycles elapsed since power-on.
    pub fn mcycles(&self) -> u64 {
        self.mcycles
    }

    /// Advance every component by one M-cycle.
    ///
    /// The order is load-bearing: timer, serial, DMA, PPU, APU, cartridge,
    /// then the CPU's access (in `CpuBus` read/write). The mooneye timer and
    /// OAM DMA tests and `bus_tests.rs` pin it down, so don't reorder casually.
    #[inline]
    pub(crate) fn tick_components(&mut self) {
        if self.slow {
            return self.tick_slow(false);
        }
        self.mcycles += 1;
        self.timer.tick_mcycle(&mut self.interrupts);
        self.serial
            .tick_mcycle(&mut self.interrupts, self.timer.div_counter());
        self.step_dma();
        self.ppu.tick_mcycle(&mut self.interrupts);
        self.apu.tick_mcycle(self.timer.div_counter());
        if self.cart_ticks {
            self.cart.tick_mcycle();
        }
    }

    /// Copy one OAM DMA byte if a transfer is active (Pan Docs: "OAM DMA Transfer").
    pub(crate) fn step_dma(&mut self) {
        if let Some((src, index)) = self.dma.tick_mcycle() {
            let byte = self.read_dma_source(src);
            self.dma.latch_data(byte);
            self.ppu.oam_dma_write(index, byte);
        }
    }
}

impl CpuBus for Bus {
    fn read(&mut self, addr: u16) -> u8 {
        self.tick_components();
        self.cpu_read(addr)
    }

    fn write(&mut self, addr: u16, value: u8) {
        self.tick_components();
        self.cpu_write(addr, value);
    }

    fn tick(&mut self) {
        self.tick_components();
    }

    fn pending_interrupts(&self) -> u8 {
        self.interrupts.pending()
    }

    fn acknowledge_interrupt(&mut self, mask: u8) {
        self.interrupts.acknowledge(mask);
    }

    fn stop(&mut self) -> bool {
        if self.hw == Hw::Cgb && self.key1_prepare && !self.joypad.any_selected_low() {
            self.speed_switch();
            return true;
        }
        self.timer.reset_div(&mut self.interrupts);
        false
    }

    fn idle_tick(&mut self) {
        if self.slow {
            self.tick_slow(true);
        } else {
            self.tick_components();
        }
    }

    fn joypad_line_low(&self) -> bool {
        self.joypad.any_selected_low()
    }
}

#[cfg(test)]
#[path = "bus_tests.rs"]
mod tests;
impl Bus {
    /// Every non-CPU component, each in its own tagged section.
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.u8(tag::BUS);
        w.bytes(&self.wram[..self.wram_len()]);
        w.bytes(&self.hram);
        w.u64(self.mcycles);
        // CGB additions.
        if self.hw.is_cgb() {
            w.u8(self.svbk);
            w.bool(self.key1_prepare);
            self.cgb_scratch.save_state(w);
        }
        if self.hw == Hw::Cgb {
            w.bool(self.double_speed);
            self.hdma.save_state(w);
        }
        w.u8(tag::INTERRUPTS);
        self.interrupts.save_state(w);
        w.u8(tag::TIMER);
        self.timer.save_state(w);
        w.u8(tag::PPU);
        self.ppu.save_state(w);
        w.u8(tag::DMA);
        self.dma.save_state(w);
        w.u8(tag::APU);
        self.apu.save_state(w);
        w.u8(tag::JOYPAD);
        self.joypad.save_state(w);
        w.u8(tag::SERIAL);
        self.serial.save_state(w);
        w.u8(tag::CART);
        self.cart.save_state(w);
    }

    /// Restore from [`Bus::save_state`] output. May leave the bus partly
    /// overwritten on error; [`crate::GameBoy::load_state`] rolls it back.
    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.tag(tag::BUS)?;
        let len = self.wram_len();
        r.bytes_into(&mut self.wram[..len])?;
        r.bytes_into(&mut self.hram)?;
        self.mcycles = r.u64()?;
        if self.hw.is_cgb() {
            let svbk = r.u8()?;
            check(svbk < 8, "SVBK")?;
            self.set_svbk(svbk);
            self.key1_prepare = r.bool()?;
            self.cgb_scratch = CgbScratch::load_state(r, self.hw == Hw::Cgb)?;
        }
        if self.hw == Hw::Cgb {
            self.double_speed = r.bool()?;
            self.hdma = Hdma::load_state(r)?;
        }
        r.tag(tag::INTERRUPTS)?;
        self.interrupts.load_state(r)?;
        r.tag(tag::TIMER)?;
        self.timer.load_state(r)?;
        r.tag(tag::PPU)?;
        self.ppu.load_state(r)?;
        r.tag(tag::DMA)?;
        self.dma.load_state(r)?;
        r.tag(tag::APU)?;
        self.apu.load_state(r)?;
        r.tag(tag::JOYPAD)?;
        self.joypad.load_state(r)?;
        r.tag(tag::SERIAL)?;
        self.serial.load_state(r)?;
        r.tag(tag::CART)?;
        self.cart.load_state(r)?;
        // Derived from what was just loaded (neither is part of a state).
        self.ppu.set_double_speed(self.double_speed);
        self.refresh_slow();
        Ok(())
    }
}
