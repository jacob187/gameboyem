//! Helpers for the `apu_*` integration tests: a bare `Bus` (no CPU) driven
//! through its register interface, with all four PSG channels and both Direct
//! Sound FIFOs (timer 0/1 + DMA 1/2 refills) running.
#![allow(dead_code)]

use gba_core::CYCLES_PER_FRAME;
use gba_core::bios::Bios;
use gba_core::bus::{Access, Bus, CpuBus};
use gba_core::cart::Cart;

pub const N: Access = Access::NonSeq;
pub const IWRAM: u32 = 0x0300_0000;
pub const FIFO_A: u32 = 0x0400_00A0;
pub const FIFO_B: u32 = 0x0400_00A4;

pub fn new_bus() -> Bus {
    Bus::new(Cart::from_rom(vec![0; 0x200]).unwrap(), Bios::Hle)
}

/// Run the machine for `cycles`: DMA runs when active, otherwise the clock
/// idles in small steps; events are dispatched after each.
pub fn run(bus: &mut Bus, cycles: u64) {
    let end = bus.sched.now() + cycles;
    while bus.sched.now() < end {
        if bus.dma.active_channel().is_some() {
            bus.run_dma();
        } else {
            bus.idle(((end - bus.sched.now()).min(48)) as u32);
        }
        bus.process_events();
    }
}

/// Run one frame's worth of cycles and append the audio it produced.
pub fn run_frame_audio(bus: &mut Bus, out: &mut Vec<f32>) {
    run(bus, u64::from(CYCLES_PER_FRAME));
    bus.apu.advance_to(bus.sched.now());
    bus.apu.drain_samples(out);
}

/// All four PSG channels and both FIFOs playing. FIFO A is fed from IWRAM by
/// DMA 1 and timer 0 (32.768 kHz), FIFO B from IWRAM + 0x4000 by DMA 2 and
/// timer 1 (16.384 kHz).
pub fn start_scene(bus: &mut Bus) {
    for i in 0..0x1000u32 {
        let a = (i * 4).wrapping_mul(5) & 0xFF;
        let word = |k: u32| u32::from((a + k * 5) as u8);
        let v = word(0) | word(1) << 8 | word(2) << 16 | word(3) << 24;
        bus.write32(IWRAM + 4 * i, v, N);
        bus.write32(IWRAM + 0x4000 + 4 * i, !v, N);
    }
    let w = |bus: &mut Bus, off: u32, v: u16| bus.write16(0x0400_0000 + off, v, N);
    w(bus, 0x84, 0x80);
    w(bus, 0x80, 0xFF77);
    // PSG 100%, A and B 100% on both sides, A timer 0, B timer 1.
    w(
        bus,
        0x82,
        0x0002 | 0x0004 | 0x0008 | 0x0300 | 0x3000 | 0x4000,
    );
    // Channel 1: sweep up, 25% duty, envelope decay.
    w(bus, 0x60, 0x0079);
    w(bus, 0x62, 0xA741);
    w(bus, 0x64, 0x8500);
    // Channel 2: 50% duty, steady.
    w(bus, 0x68, 0xC080);
    w(bus, 0x6C, 0x8600);
    // Channel 3: wave bank 1 filled by the CPU, 64 digits.
    w(bus, 0x70, 0x40);
    for i in 0..8u16 {
        w(bus, 0x90 + 2 * u32::from(i), 0x2468u16.wrapping_mul(i + 1));
    }
    w(bus, 0x70, 0xA0);
    w(bus, 0x72, 0x2000);
    w(bus, 0x74, 0x8700);
    // Channel 4: 15-bit noise, medium rate.
    w(bus, 0x78, 0xB200);
    w(bus, 0x7C, 0x8052);
    // FIFOs: prime, then DMA special timing, repeat, destination fixed.
    bus.write32(FIFO_A, 0x0303_0303, N);
    bus.write32(FIFO_B, 0x0404_0404, N);
    let dma = |bus: &mut Bus, ch: u32, src: u32, dst: u32| {
        let base = 0x0400_00B0 + ch * 12;
        bus.write32(base, src, N);
        bus.write32(base + 4, dst, N);
        bus.write32(
            base + 8,
            4 | u32::from(0x8000u16 | 3 << 12 | 1 << 9 | 2 << 5) << 16,
            N,
        );
    };
    dma(bus, 1, IWRAM, FIFO_A);
    dma(bus, 2, IWRAM + 0x4000, FIFO_B);
    // Timers: overflow every 512 cycles (A) and 1024 cycles (B).
    bus.write32(0x0400_0100, 0xFE00 | 0x80 << 16, N);
    bus.write32(0x0400_0104, 0xFC00 | 0x80 << 16, N);
}

/// FNV-1a over the little-endian bytes of each sample's bit pattern.
pub fn hash_audio(samples: &[f32]) -> u64 {
    let bytes: Vec<u8> = samples
        .iter()
        .flat_map(|s| s.to_bits().to_le_bytes())
        .collect();
    gba_core::fnv1a(&bytes)
}

/// `frames` frames of the scene's audio from power-on, and the cycles run.
pub fn scene_audio(frames: usize) -> (Vec<f32>, u64) {
    let mut bus = new_bus();
    start_scene(&mut bus);
    let mut out = vec![];
    for _ in 0..frames {
        run_frame_audio(&mut bus, &mut out);
    }
    (out, bus.sched.now())
}
