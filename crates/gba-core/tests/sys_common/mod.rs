//! Helpers shared by the `sys_*` integration tests (include with `mod`).
#![allow(dead_code)]

use gba_core::bios::Bios;
use gba_core::bus::{Access, Bus, CpuBus};
use gba_core::cart::Cart;

pub const N: Access = Access::NonSeq;
pub const S: Access = Access::Seq;

/// A 0x200-byte ROM (header zeros) with `extra` written at 0x100.
pub fn rom_with(extra: &[u8]) -> Vec<u8> {
    let mut rom = vec![0; 0x200];
    rom[0x100..0x100 + extra.len()].copy_from_slice(extra);
    rom
}

pub fn bus_with(rom: Vec<u8>) -> Bus {
    Bus::new(Cart::from_rom(rom).unwrap(), Bios::Hle)
}

pub fn bus() -> Bus {
    bus_with(rom_with(&[]))
}

/// Cycles the closure's bus accesses cost.
pub fn cycles<R>(bus: &mut Bus, f: impl FnOnce(&mut Bus) -> R) -> u64 {
    let t = bus.sched.now();
    f(bus);
    bus.sched.now() - t
}

/// Run DMA and dispatch events until no channel is active.
pub fn settle(bus: &mut Bus) {
    for _ in 0..1_000_000 {
        if bus.dma.active_channel().is_none() {
            return;
        }
        bus.run_dma();
        bus.process_events();
    }
    panic!("DMA never finished");
}

/// Advance the clock by `n` cycles and dispatch what fell due.
pub fn run(bus: &mut Bus, n: u64) {
    bus.idle(n as u32);
    bus.process_events();
}

/// Advance the clock to absolute time `t` (a no-op if already past) and dispatch.
pub fn run_to(bus: &mut Bus, t: u64) {
    let now = bus.sched.now();
    if t > now {
        run(bus, t - now);
    } else {
        bus.process_events();
    }
}

pub const DMA3: u32 = 0x0400_00D4;

/// Program DMA channel `ch` (0..=3) through 32-bit bus writes, enable bit last.
pub fn start_dma(bus: &mut Bus, ch: u32, src: u32, dst: u32, count: u16, control: u16) {
    let base = 0x0400_00B0 + ch * 12;
    bus.write32(base, src, N);
    bus.write32(base + 4, dst, N);
    bus.write32(base + 8, u32::from(count) | u32::from(control) << 16, N);
}
