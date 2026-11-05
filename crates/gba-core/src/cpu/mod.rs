//! The ARM7TDMI (ARMv4T: ARM + Thumb). Owned by P1-CPU.
//!
//! **Contract:** [`Cpu::reset`], [`Cpu::post_bios`], [`Cpu::step`],
//! [`Cpu::registers`] and [`Registers`]. The CPU is generic over
//! [`CpuBus`](crate::bus::CpuBus), charges every access through it with the right
//! [`Access`](crate::bus::Access) kind, and checks `bus.irq_line()` (plus CPSR.I)
//! before each instruction. SWIs go to `bus.swi` first (HLE BIOS); on
//! `Unhandled` the CPU takes the exception.

use crate::bus::CpuBus;

/// A snapshot of the registers visible in the current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Registers {
    /// r0..=r15 as the current mode sees them. `r[15]` is the address of the next
    /// instruction to execute (not the pipelined +8/+4 value).
    pub r: [u32; 16],
    pub cpsr: u32,
}

pub struct Cpu {
    regs: Registers,
}

impl Cpu {
    /// Power-on state: Supervisor mode, IRQ and FIQ masked, ARM state, PC = 0
    /// (boots a real BIOS).
    pub fn reset() -> Self {
        Self {
            regs: Registers {
                r: [0; 16],
                cpsr: 0xD3,
            },
        }
    }

    /// The state the BIOS hands a cartridge: System mode, ARM state, PC =
    /// 0x0800_0000, SP_usr/sys = 0x0300_7F00, SP_irq = 0x0300_7FA0, SP_svc =
    /// 0x0300_7FE0 (GBATEK "BIOS RAM Usage"). Used with the HLE BIOS.
    pub fn post_bios() -> Self {
        let mut r = [0; 16];
        r[13] = 0x0300_7F00;
        r[15] = 0x0800_0000;
        // P1-CPU: also set the banked IRQ/SVC stack pointers.
        Self {
            regs: Registers { r, cpsr: 0x1F },
        }
    }

    /// Execute one instruction (or take a pending IRQ).
    pub fn step<B: CpuBus>(&mut self, bus: &mut B) {
        // Placeholder until P1-CPU: burn one cycle so the run loop advances.
        bus.idle(1);
    }

    pub fn registers(&self) -> Registers {
        self.regs
    }
}
