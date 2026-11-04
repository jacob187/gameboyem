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
use crate::cartridge::Cartridge;
use crate::dma::Dma;
use crate::interrupts::Interrupts;
use crate::joypad::Joypad;
use crate::ppu::Ppu;
use crate::serial::Serial;
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
    /// Called when the CPU executes `STOP` (DMG: resets DIV). No cycle cost.
    fn stop(&mut self) {}
    /// True while any *selected* P1 input line is low (button held). This is
    /// the STOP wake-up condition and is independent of IE. No cycle cost.
    fn joypad_line_low(&self) -> bool {
        false
    }
}

pub(crate) const WRAM_SIZE: usize = 0x2000;
pub(crate) const HRAM_SIZE: usize = 0x7F;

/// Owns all non-CPU components and routes memory accesses (see `mmu.rs`).
pub struct Bus {
    pub(crate) cart: Box<dyn Cartridge>,
    pub(crate) wram: Box<[u8; WRAM_SIZE]>,
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
}

impl Bus {
    pub fn new(cart: Box<dyn Cartridge>) -> Self {
        Self {
            cart,
            wram: Box::new([0; WRAM_SIZE]),
            hram: [0; HRAM_SIZE],
            interrupts: Interrupts::new(),
            timer: Timer::new(),
            ppu: Ppu::new(),
            dma: Dma::new(),
            apu: Apu::new(),
            joypad: Joypad::new(),
            serial: Serial::new(),
            mcycles: 0,
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
    pub(crate) fn tick_components(&mut self) {
        self.mcycles += 1;
        self.timer.tick_mcycle(&mut self.interrupts);
        self.serial.tick_mcycle(&mut self.interrupts);
        self.step_dma();
        self.ppu.tick_mcycle(&mut self.interrupts);
        self.apu.tick_mcycle(self.timer.div_counter());
        self.cart.tick_mcycle();
    }

    /// Copy one OAM DMA byte if a transfer is active (Pan Docs: "OAM DMA Transfer").
    fn step_dma(&mut self) {
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

    fn stop(&mut self) {
        self.timer.reset_div(&mut self.interrupts);
    }

    fn joypad_line_low(&self) -> bool {
        self.joypad.any_selected_low()
    }
}

#[cfg(test)]
#[path = "bus_tests.rs"]
mod tests;
