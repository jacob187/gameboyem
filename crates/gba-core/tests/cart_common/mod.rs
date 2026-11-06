//! GPIO/RTC bit-banging helpers shared by the `cart_*` integration tests (include
//! with `mod`, after `mod sys_common`): a game driver's 16-bit stores to
//! 0x0800_00C4/C6/C8, command byte MSB first, data bytes LSB first.
#![allow(dead_code)]

use crate::sys_common::*;
use gba_core::bios::Bios;
use gba_core::bus::{Bus, CpuBus};
use gba_core::cart::{Cart, CartOptions, RtcMode};

pub const DATA: u32 = 0x0800_00C4;
pub const DIR: u32 = 0x0800_00C6;
pub const CTRL: u32 = 0x0800_00C8;
pub const SCK: u16 = 1;
pub const SIO: u16 = 2;
pub const CS: u16 = 4;
pub const CPS: u64 = 16_777_216;

/// A ROM carrying the library string `id`, whose bytes at 0xC4..0xCA are recognisable.
pub fn rom_for(id: &[u8]) -> Vec<u8> {
    let mut r = rom_with(id);
    r[0xC4..0xCA].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
    r
}

pub fn bus_in(mode: RtcMode) -> Bus {
    bus_for(&[], mode)
}

pub fn bus_for(id: &[u8], mode: RtcMode) -> Bus {
    let cart = Cart::with_options(
        rom_for(id),
        CartOptions {
            rtc: mode,
            ..CartOptions::default()
        },
    )
    .unwrap();
    Bus::new(cart, Bios::Hle)
}

/// The driver's init: data 1, then 5 (CS up while SCK is high), all pins out,
/// reads enabled.
pub fn init(b: &mut Bus) {
    b.write16(DATA, 1, N);
    b.write16(DATA, 5, N);
    b.write16(DIR, 7, N);
    b.write16(CTRL, 1, N);
}

pub fn bit_out(b: &mut Bus, bit: u16) {
    b.write16(DATA, bit << 1 | CS, N);
    b.write16(DATA, bit << 1 | CS | SCK, N);
}

pub fn send_command(b: &mut Bus, reg: u8, read: bool) {
    b.write16(DIR, 7, N);
    let byte = 0x60 | reg << 1 | u8::from(read);
    for i in (0..8).rev() {
        bit_out(b, u16::from(byte >> i & 1));
    }
}

pub fn write_data(b: &mut Bus, bytes: &[u8]) {
    for byte in bytes {
        for i in 0..8 {
            bit_out(b, u16::from(byte >> i & 1));
        }
    }
}

pub fn read_data(b: &mut Bus, n: usize) -> Vec<u8> {
    b.write16(DIR, 5, N); // SIO becomes an input
    (0..n)
        .map(|_| {
            (0..8).fold(0u8, |v, i| {
                b.write16(DATA, CS, N);
                b.write16(DATA, CS | SCK, N);
                v | ((((b.read16(DATA, N) & SIO) >> 1) as u8) << i)
            })
        })
        .collect()
}

/// A whole transfer: CS up, command, then `body`, CS down.
pub fn transfer(
    b: &mut Bus,
    reg: u8,
    read: bool,
    body: impl FnOnce(&mut Bus) -> Vec<u8>,
) -> Vec<u8> {
    b.write16(DATA, SCK, N);
    b.write16(DATA, SCK | CS, N);
    send_command(b, reg, read);
    let out = body(b);
    b.write16(DATA, SCK, N);
    b.write16(DIR, 7, N);
    out
}

pub fn read_reg(b: &mut Bus, reg: u8, n: usize) -> Vec<u8> {
    transfer(b, reg, true, |b| read_data(b, n))
}

pub fn write_reg(b: &mut Bus, reg: u8, bytes: &[u8]) {
    transfer(b, reg, false, |b| {
        write_data(b, bytes);
        vec![]
    });
}

pub fn reset(b: &mut Bus) {
    transfer(b, 0, false, |_| vec![]);
}
