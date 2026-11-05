//! The BIOS: either the built-in high-level emulation (the default, DECISIONS #47)
//! or a user-supplied 16 KiB dump. Built in phase P2 (P2-BIOS).
//!
//! **Contract:** [`Bios`], [`Bios::read8`], and [`swi`], which the bus's
//! `CpuBus::swi` forwards to.

use crate::bus::{Bus, SwiResult};

pub enum Bios {
    /// HLE: SWIs are emulated in Rust; the BIOS region holds a small built-in image
    /// (IRQ vector stub etc., written in P2) instead of Nintendo's code.
    Hle,
    /// A real BIOS dump (exactly 16 KiB, validated by the host).
    Real(Box<[u8]>),
}

impl Bios {
    /// BIOS byte at `addr` (0..0x4000). P2-BIOS adds read protection (reads from
    /// outside the BIOS return the last fetched BIOS opcode).
    pub fn read8(&self, addr: u32) -> u8 {
        match self {
            Bios::Hle => 0,
            Bios::Real(data) => data.get(addr as usize).copied().unwrap_or(0),
        }
    }
}

/// Handle SWI `number` with r0..=r3 in `r`. A real BIOS runs its own code.
pub fn swi(bus: &mut Bus, number: u8, r: &mut [u32; 4]) -> SwiResult {
    let _ = (number, r);
    match bus.bios {
        Bios::Real(_) => SwiResult::Unhandled,
        Bios::Hle => SwiResult::Unhandled, // P2-BIOS
    }
}
