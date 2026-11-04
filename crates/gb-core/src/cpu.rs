//! Sharp SM83 CPU (Pan Docs: "CPU Registers and Flags", "CPU Instruction Set",
//! "Interrupts", "halt").
//!
//! # Contract (frozen; propose changes to the orchestrator)
//!
//! * [`Cpu::step`] executes exactly one of: an interrupt dispatch, one halted
//!   M-cycle, or one instruction. All cycle cost is paid through the
//!   [`CpuBus`] (`read`/`write`/`tick`, one M-cycle each).
//! * Opcode `0x40` (`LD B,B`) sets a flag readable with
//!   [`Cpu::take_ld_b_b_breakpoint`] (mooneye/acid2 test signal).
//! * Illegal opcodes (0xD3, 0xDB, ...) lock the CPU: [`Cpu::is_locked`] becomes
//!   true and subsequent steps only tick the bus.
//!
//! Phase 1 stub: only `NOP` and `LD B,B`; every other opcode locks up.

use crate::bus::CpuBus;

/// SM83 register file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Registers {
    pub a: u8,
    /// Flags: Z=0x80, N=0x40, H=0x20, C=0x10; low nibble always 0.
    pub f: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    pub sp: u16,
    pub pc: u16,
}

impl Registers {
    /// DMG register state after the boot ROM (Pan Docs: "Power Up Sequence").
    pub const fn dmg_post_boot() -> Self {
        Self {
            a: 0x01,
            f: 0xB0,
            b: 0x00,
            c: 0x13,
            d: 0x00,
            e: 0xD8,
            h: 0x01,
            l: 0x4D,
            sp: 0xFFFE,
            pc: 0x0100,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Cpu {
    pub regs: Registers,
    locked: bool,
    ld_b_b: bool,
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu {
    pub fn new() -> Self {
        Self {
            regs: Registers::dmg_post_boot(),
            locked: false,
            ld_b_b: false,
        }
    }

    pub fn registers(&self) -> &Registers {
        &self.regs
    }

    /// True after an illegal opcode hung the CPU.
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Returns true (once) if `LD B,B` executed since the last call.
    pub fn take_ld_b_b_breakpoint(&mut self) -> bool {
        std::mem::take(&mut self.ld_b_b)
    }

    pub fn step<B: CpuBus>(&mut self, bus: &mut B) {
        if self.locked {
            bus.tick();
            return;
        }
        let op = bus.read(self.regs.pc);
        self.regs.pc = self.regs.pc.wrapping_add(1);
        match op {
            0x00 => {}
            0x40 => self.ld_b_b = true,
            _ => self.locked = true,
        }
    }
}
