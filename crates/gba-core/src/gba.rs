//! The public emulator facade and the run loop. Owned by P1-SYS.

use crate::bios::Bios;
use crate::bus::Bus;
use crate::cart::{Cart, CartError};
use crate::cpu::{Cpu, Registers};
use crate::keypad::Button;
use crate::{CYCLES_PER_FRAME, Framebuffer};

/// A complete Game Boy Advance: CPU + bus (which owns every other component).
pub struct Gba {
    cpu: Cpu,
    bus: Bus,
    /// FNV-1a of the ROM image; ties save states and recordings to their ROM.
    rom_hash: u64,
}

impl Gba {
    /// Power on with `rom` inserted. With `bios = None` the built-in HLE BIOS is
    /// used and the machine starts in the post-BIOS state at the cartridge entry
    /// point; with a real 16 KiB dump it boots through the BIOS from address 0.
    /// The host validates the dump's size (and may check its hash).
    pub fn new(rom: Vec<u8>, bios: Option<Box<[u8]>>) -> Result<Self, CartError> {
        let rom_hash = crate::fnv1a(&rom);
        let cart = Cart::from_rom(rom)?;
        let (cpu, bios) = match bios {
            Some(data) => (Cpu::reset(), Bios::Real(data)),
            None => (Cpu::post_bios(), Bios::Hle),
        };
        Ok(Self {
            cpu,
            bus: Bus::new(cart, bios),
            rom_hash,
        })
    }

    /// Run until the next VBlank starts (one frame), with a two-frame safety cap.
    pub fn step_frame(&mut self) {
        let start = self.bus.frames;
        let cap = self.bus.sched.now() + 2 * u64::from(CYCLES_PER_FRAME);
        while self.bus.frames == start && self.bus.sched.now() < cap {
            if !self.step_once() {
                break;
            }
        }
    }

    /// One step of the run loop: an active DMA (which holds the bus, even while
    /// the CPU is halted), else the clock jumps to the next event while halted,
    /// else one CPU instruction. Due events are dispatched afterwards. Returns
    /// false if the machine is halted with nothing scheduled to wake it.
    fn step_once(&mut self) -> bool {
        if self.bus.dma.active_channel().is_some() {
            self.bus.run_dma();
        } else if self.bus.halted {
            if !self.bus.sched.skip_to_next() {
                return false;
            }
        } else {
            self.cpu.step(&mut self.bus);
        }
        self.bus.process_events();
        true
    }

    /// Execute one CPU instruction, DMA burst or halt skip, and dispatch due
    /// events (test hook).
    pub fn step_instruction(&mut self) {
        self.step_once();
    }

    pub fn framebuffer(&self) -> &Framebuffer {
        self.bus.ppu.framebuffer()
    }

    pub fn set_button(&mut self, b: Button, pressed: bool) {
        self.bus.set_button(b, pressed);
    }

    /// The cartridge title from the header.
    pub fn title(&self) -> String {
        self.bus.cart.title()
    }

    pub fn rom_hash(&self) -> u64 {
        self.rom_hash
    }

    /// CPU registers (test hook).
    pub fn cpu_registers(&self) -> Registers {
        self.cpu.registers()
    }

    /// Read a byte with no side effects (test hook).
    pub fn peek8(&self, addr: u32) -> u8 {
        self.bus.peek8(addr)
    }

    /// Read a halfword (`addr & !1`) with no side effects (test hook).
    pub fn peek16(&self, addr: u32) -> u16 {
        self.bus.peek16(addr)
    }

    /// Read a word (`addr & !3`) with no side effects (test hook).
    pub fn peek32(&self, addr: u32) -> u32 {
        self.bus.peek32(addr)
    }

    /// The save data to write to a `.sav` file: SRAM, Flash, or EEPROM (`None`
    /// without a backup chip, or for an EEPROM the game hasn't touched yet, whose
    /// size is still unknown).
    pub fn battery_ram(&self) -> Option<&[u8]> {
        self.bus.cart.battery_ram()
    }

    /// Counter that advances only when the game changes save data (or a save is
    /// loaded), so a host can skip rewriting an unchanged `.sav`.
    pub fn battery_generation(&self) -> u64 {
        self.bus.cart.battery_generation()
    }

    /// Restore save data from a `.sav` file. The size must match the detected
    /// backup chip (an EEPROM accepts 512 or 8192 bytes).
    pub fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), CartError> {
        self.bus.cart.load_battery_ram(data)
    }

    /// Master clock in CPU cycles since power-on.
    pub fn cycles(&self) -> u64 {
        self.bus.sched.now()
    }

    /// Frames completed since power-on.
    pub fn frames(&self) -> u64 {
        self.bus.frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_frame_runs_one_frame_of_cycles() {
        let mut gba = Gba::new(vec![0; 0x200], None).unwrap();
        gba.step_frame();
        assert_eq!(gba.frames(), 1);
        // The first VBlank starts at line 160.
        assert_eq!(
            gba.cycles(),
            u64::from(crate::CYCLES_PER_LINE * crate::VISIBLE_LINES)
        );
        gba.step_frame();
        assert_eq!(gba.frames(), 2);
        assert_eq!(
            gba.cycles(),
            u64::from(crate::CYCLES_PER_LINE * crate::VISIBLE_LINES + CYCLES_PER_FRAME)
        );
    }
}
