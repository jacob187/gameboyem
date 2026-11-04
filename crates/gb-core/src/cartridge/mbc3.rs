//! MBC3 (Pan Docs: "MBC3"). Up to 2 MiB ROM / 32 KiB RAM plus a real-time clock.
//!
//! The MBC30 variant (Japanese Pokemon Crystal) has an 8-bit ROM bank (4 MiB)
//! and 8 RAM banks (64 KiB). It shares cartridge types 0x0F-0x13 with MBC3, so
//! it is detected from the header: the declared ROM size is above 2 MiB or the
//! declared RAM size is 64 KiB. Anything else keeps the plain MBC3 masks
//! (7-bit ROM bank, RAM banks 0-3).
//!
//! Registers:
//! * `0000-1FFF` RAM and RTC enable (low nibble `0xA` enables).
//! * `2000-3FFF` ROM bank, 7 bits (MBC30: 8); a written 0 is treated as 1.
//! * `4000-5FFF` `00-03` (MBC30: `00-07`) selects a RAM bank, `08-0C` selects
//!   an RTC register (S, M, H, DL, DH) for `A000-BFFF`.
//! * `6000-7FFF` latch: writing `00` then `01` copies the live clock into the
//!   latched registers that reads return.
//!
//! The RTC is driven only by [`Cartridge::tick_mcycle`] (emulated time, 2^20
//! M-cycles per second); the host clock is never consulted, so runs are
//! deterministic and the clock does not advance while the emulator is stopped.
//!
//! Save format: the RAM bytes, optionally followed (RTC cartridges only) by the
//! 48-byte footer used by VBA/BGB: live S, M, H, DL, DH then latched S, M, H,
//! DL, DH as little-endian `u32`s, then a little-endian `u64` wall-clock
//! timestamp. We always write 0 for the timestamp (no host clock) and ignore it
//! on load. Loading also accepts the 44-byte variant (32-bit timestamp) and a
//! bare RAM image with no footer (clock left at zero).

use super::{
    Cartridge, CartridgeError, CartridgeHeader, RAM_BANK_SIZE, ROM_BANK_SIZE, ram_window_offset,
};

/// M-cycles per emulated second (4 194 304 Hz / 4).
const MCYCLES_PER_SECOND: u32 = 1 << 20;
const RTC_FOOTER_LEN: usize = 48;
const RTC_FOOTER_LEN_SHORT: usize = 44;

const DH_DAY_HIGH: u8 = 0x01;
const DH_HALT: u8 = 0x40;
const DH_CARRY: u8 = 0x80;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Rtc {
    s: u8,
    m: u8,
    h: u8,
    dl: u8,
    /// Bit 0 = day counter bit 8, bit 6 = halt, bit 7 = day carry.
    dh: u8,
}

impl Rtc {
    fn get(&self, reg: u8) -> u8 {
        match reg {
            0x08 => self.s,
            0x09 => self.m,
            0x0A => self.h,
            0x0B => self.dl,
            _ => self.dh,
        }
    }

    fn set(&mut self, reg: u8, value: u8) {
        match reg {
            0x08 => self.s = value & 0x3F,
            0x09 => self.m = value & 0x3F,
            0x0A => self.h = value & 0x1F,
            0x0B => self.dl = value,
            _ => self.dh = value & (DH_DAY_HIGH | DH_HALT | DH_CARRY),
        }
    }

    fn halted(&self) -> bool {
        self.dh & DH_HALT != 0
    }

    /// Advance by one second with the hardware's wrap rules: each field rolls
    /// over at its natural limit, but out-of-range values written by software
    /// count on to their bit-width limit without carrying.
    fn tick_second(&mut self) {
        self.s = (self.s + 1) & 0x3F;
        if self.s != 60 {
            return;
        }
        self.s = 0;
        self.m = (self.m + 1) & 0x3F;
        if self.m != 60 {
            return;
        }
        self.m = 0;
        self.h = (self.h + 1) & 0x1F;
        if self.h != 24 {
            return;
        }
        self.h = 0;
        self.tick_day();
    }

    fn tick_day(&mut self) {
        let day = (((self.dh & DH_DAY_HIGH) as u16) << 8 | self.dl as u16) + 1;
        if day == 0x200 {
            self.dh |= DH_CARRY;
        }
        let day = day & 0x1FF;
        self.dl = day as u8;
        self.dh = (self.dh & !DH_DAY_HIGH) | (day >> 8) as u8;
    }

    fn to_regs(self) -> [u8; 5] {
        [self.s, self.m, self.h, self.dl, self.dh]
    }

    fn from_regs(regs: [u32; 5]) -> Self {
        let mut r = Self::default();
        for (i, v) in regs.into_iter().enumerate() {
            r.set(0x08 + i as u8, v as u8);
        }
        r
    }
}

/// MBC30 detection (see module docs): more than 2 MiB of ROM or 64 KiB of RAM.
fn is_mbc30(header: &CartridgeHeader) -> bool {
    header.rom_size > 0x20_0000 || header.ram_size == 0x1_0000
}

pub struct Mbc3 {
    rom: Vec<u8>,
    ram: Vec<u8>,
    header: CartridgeHeader,
    /// See [`Cartridge::battery_generation`].
    battery_gen: u64,
    rom_bank_mask: usize,
    /// Mask applied to values written to the ROM bank register (0x7F or 0xFF).
    rom_bank_reg_mask: u8,
    /// Highest RAM bank selector (3 for MBC3, 7 for MBC30).
    max_ram_select: u8,
    ram_enabled: bool,
    rom_bank: u8,
    /// Raw value of the `4000-5FFF` register.
    ram_select: u8,
    live: Rtc,
    latched: Rtc,
    /// M-cycles accumulated towards the next RTC second.
    subsecond: u32,
    /// A `00` was written to the latch register and `01` would now latch.
    latch_armed: bool,
}

impl Mbc3 {
    pub fn new(rom: Vec<u8>, header: CartridgeHeader) -> Self {
        let ram = vec![0; header.ram_size];
        let rom_bank_mask = rom.len() / ROM_BANK_SIZE - 1;
        let mbc30 = is_mbc30(&header);
        Self {
            rom,
            ram,
            header,
            battery_gen: 0,
            rom_bank_mask,
            rom_bank_reg_mask: if mbc30 { 0xFF } else { 0x7F },
            max_ram_select: if mbc30 { 7 } else { 3 },
            ram_enabled: false,
            rom_bank: 1,
            ram_select: 0,
            live: Rtc::default(),
            latched: Rtc::default(),
            subsecond: 0,
            latch_armed: false,
        }
    }

    fn ram_offset(&self, addr: u16) -> Option<usize> {
        if self.ram.is_empty() {
            return None;
        }
        let offset = self.ram_select as usize * RAM_BANK_SIZE + ram_window_offset(addr);
        Some(offset % self.ram.len())
    }
}

impl Cartridge for Mbc3 {
    fn header(&self) -> &CartridgeHeader {
        &self.header
    }

    fn read_rom(&self, addr: u16) -> u8 {
        let bank = if addr < 0x4000 {
            0
        } else {
            self.rom_bank as usize & self.rom_bank_mask
        };
        self.rom[bank * ROM_BANK_SIZE + (addr as usize & 0x3FFF)]
    }

    fn write_rom(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000..=0x1FFF => self.ram_enabled = value & 0x0F == 0x0A,
            0x2000..=0x3FFF => {
                let v = value & self.rom_bank_reg_mask;
                self.rom_bank = if v == 0 { 1 } else { v };
            }
            0x4000..=0x5FFF => self.ram_select = value,
            _ => {
                if value == 0 {
                    self.latch_armed = true;
                } else {
                    if value == 1 && self.latch_armed && self.header.has_rtc {
                        if self.latched != self.live {
                            self.battery_gen += 1;
                        }
                        self.latched = self.live;
                    }
                    self.latch_armed = false;
                }
            }
        }
    }

    fn read_ram(&self, addr: u16) -> u8 {
        if !self.ram_enabled {
            return 0xFF;
        }
        match self.ram_select {
            0x00..=0x07 if self.ram_select <= self.max_ram_select => {
                self.ram_offset(addr).map_or(0xFF, |o| self.ram[o])
            }
            0x08..=0x0C if self.header.has_rtc => self.latched.get(self.ram_select),
            _ => 0xFF,
        }
    }

    fn write_ram(&mut self, addr: u16, value: u8) {
        if !self.ram_enabled {
            return;
        }
        match self.ram_select {
            0x00..=0x07 if self.ram_select <= self.max_ram_select => {
                if let Some(o) = self.ram_offset(addr) {
                    if self.ram[o] != value {
                        self.ram[o] = value;
                        self.battery_gen += 1;
                    }
                }
            }
            0x08..=0x0C if self.header.has_rtc => {
                if self.ram_select == 0x08 {
                    // Writing seconds resets the sub-second prescaler.
                    self.subsecond = 0;
                }
                self.live.set(self.ram_select, value);
                self.battery_gen += 1;
            }
            _ => {}
        }
    }

    fn tick_mcycle(&mut self) {
        if !self.header.has_rtc || self.live.halted() {
            return;
        }
        self.subsecond += 1;
        if self.subsecond >= MCYCLES_PER_SECOND {
            self.subsecond = 0;
            self.live.tick_second();
        }
    }

    fn battery_ram(&self) -> Option<Vec<u8>> {
        if !self.header.has_battery || (self.ram.is_empty() && !self.header.has_rtc) {
            return None;
        }
        let mut out = self.ram.clone();
        if self.header.has_rtc {
            for r in self
                .live
                .to_regs()
                .into_iter()
                .chain(self.latched.to_regs())
            {
                out.extend_from_slice(&u32::from(r).to_le_bytes());
            }
            out.extend_from_slice(&0u64.to_le_bytes());
        }
        Some(out)
    }

    fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        if !self.header.has_battery {
            return Err(CartridgeError::NoBattery);
        }
        let ram_len = self.ram.len();
        let footer = if self.header.has_rtc && data.len() > ram_len {
            data.len() - ram_len
        } else {
            0
        };
        let ok = data.len() == ram_len
            || (self.header.has_rtc
                && (footer == RTC_FOOTER_LEN || footer == RTC_FOOTER_LEN_SHORT));
        if !ok {
            return Err(CartridgeError::SaveSizeMismatch {
                expected: ram_len,
                actual: data.len(),
            });
        }
        self.ram.copy_from_slice(&data[..ram_len]);
        if footer != 0 {
            let word = |i: usize| {
                let o = ram_len + i * 4;
                u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]])
            };
            self.live = Rtc::from_regs(std::array::from_fn(&word));
            self.latched = Rtc::from_regs(std::array::from_fn(|i| word(5 + i)));
            self.subsecond = 0;
        }
        self.battery_gen += 1;
        Ok(())
    }

    fn battery_generation(&self) -> u64 {
        self.battery_gen
    }
}

#[cfg(test)]
mod tests {
    use super::super::{from_rom, test_util::banked_rom};
    use super::*;

    fn tick_seconds(c: &mut dyn Cartridge, n: u32) {
        for _ in 0..n as u64 * MCYCLES_PER_SECOND as u64 {
            c.tick_mcycle();
        }
    }

    fn latch(c: &mut dyn Cartridge) {
        c.write_rom(0x6000, 0);
        c.write_rom(0x6000, 1);
    }

    fn rtc_cart() -> Box<dyn Cartridge> {
        let mut c = from_rom(banked_rom(4, 0x10, 0x01, 0x03)).unwrap();
        c.write_rom(0x0000, 0x0A);
        c
    }

    fn read_rtc(c: &mut dyn Cartridge, reg: u8) -> u8 {
        c.write_rom(0x4000, reg);
        c.read_ram(0xA000)
    }

    #[test]
    fn rom_bank_seven_bits_zero_maps_to_one() {
        let mut c = from_rom(banked_rom(128, 0x11, 0x06, 0)).unwrap();
        assert_eq!(c.read_rom(0x4000), 1);
        c.write_rom(0x2000, 0);
        assert_eq!(c.read_rom(0x4000), 1);
        c.write_rom(0x2000, 0x7F);
        assert_eq!(c.read_rom(0x4000), 0x7F);
        c.write_rom(0x3FFF, 0x80); // bit 7 ignored -> 0 -> 1
        assert_eq!(c.read_rom(0x4000), 1);
        c.write_rom(0x2000, 0xC5);
        assert_eq!(c.read_rom(0x4000), 0x45);
        assert_eq!(c.read_rom(0x0000), 0, "0000-3FFF is fixed to bank 0");
    }

    #[test]
    fn free_running_rtc_does_not_advance_battery_generation() {
        let mut c = rtc_cart();
        let g0 = c.battery_generation();
        let img0 = c.battery_ram().unwrap();
        tick_seconds(c.as_mut(), 12);
        assert_eq!(c.battery_generation(), g0, "ticking is not a save event");
        assert_ne!(c.battery_ram().unwrap(), img0, "but the image does drift");

        // A latch that changes the latched registers is.
        latch(c.as_mut());
        let g1 = c.battery_generation();
        assert!(g1 > g0);
        latch(c.as_mut());
        assert_eq!(c.battery_generation(), g1, "no-op latch: nothing changed");

        // Cart RAM write: only a changed byte counts.
        c.write_rom(0x4000, 1);
        c.write_ram(0xA000, 0x5A);
        let g2 = c.battery_generation();
        assert!(g2 > g1);
        c.write_ram(0xA000, 0x5A);
        assert_eq!(c.battery_generation(), g2);

        // RTC register write.
        c.write_rom(0x4000, 0x0A);
        c.write_ram(0xA000, 3);
        let g3 = c.battery_generation();
        assert!(g3 > g2);

        // Load restores state, so the host's baseline must move too.
        let save = c.battery_ram().unwrap();
        c.load_battery_ram(&save).unwrap();
        assert!(c.battery_generation() > g3);

        // Disabled RAM ignores writes: no generation change.
        c.write_rom(0x0000, 0x00);
        let g4 = c.battery_generation();
        c.write_ram(0xA000, 9);
        assert_eq!(c.battery_generation(), g4);
    }

    #[test]
    fn mbc30_detection() {
        let det = |banks, ct, rom_code, ram_code| {
            is_mbc30(&CartridgeHeader::parse(&banked_rom(banks, ct, rom_code, ram_code)).unwrap())
        };
        assert!(!det(2, 0x13, 0x00, 0x03), "32 KiB RAM, 32 KiB ROM");
        assert!(!det(128, 0x13, 0x06, 0x03), "exactly 2 MiB ROM is MBC3");
        assert!(!det(2, 0x13, 0x00, 0x04), "128 KiB RAM is not MBC30");
        assert!(det(256, 0x13, 0x07, 0x03), "4 MiB ROM");
        assert!(det(2, 0x13, 0x00, 0x05), "64 KiB RAM");
        assert!(det(2, 0x10, 0x00, 0x05), "64 KiB RAM with RTC");
    }

    #[test]
    fn mbc30_reaches_rom_banks_128_and_up() {
        let mut c = from_rom(banked_rom(256, 0x11, 0x07, 0)).unwrap();
        for bank in [0x80u8, 0xC5, 0xFF, 0x7F, 0x01] {
            c.write_rom(0x2000, bank);
            assert_eq!(c.read_rom(0x4000), bank, "bank {bank:#x}");
        }
        c.write_rom(0x2000, 0);
        assert_eq!(c.read_rom(0x4000), 1, "0 still maps to 1");
        assert_eq!(c.read_rom(0x0000), 0);
    }

    #[test]
    fn mbc30_reaches_ram_banks_4_to_7() {
        // RAM code 5 = 64 KiB = 8 banks.
        let mut c = from_rom(banked_rom(2, 0x13, 0, 0x05)).unwrap();
        c.write_rom(0x0000, 0x0A);
        for bank in 0..8u8 {
            c.write_rom(0x4000, bank);
            c.write_ram(0xA000, 0x10 + bank);
            c.write_ram(0xBFFF, 0x20 + bank);
        }
        for bank in 0..8u8 {
            c.write_rom(0x4000, bank);
            assert_eq!(c.read_ram(0xA000), 0x10 + bank, "bank {bank}");
            assert_eq!(c.read_ram(0xBFFF), 0x20 + bank, "bank {bank}");
        }
        c.write_rom(0x4000, 0x07);
        assert_eq!(c.battery_ram().unwrap()[7 * 0x2000], 0x17);
    }

    #[test]
    fn mbc30_rtc_registers_still_selectable() {
        let mut c = from_rom(banked_rom(2, 0x10, 0, 0x05)).unwrap();
        c.write_rom(0x0000, 0x0A);
        c.write_rom(0x4000, 0x08);
        c.write_ram(0xA000, 42);
        latch(c.as_mut());
        assert_eq!(c.read_ram(0xA000), 42);
        c.write_rom(0x4000, 0x0D); // unmapped
        assert_eq!(c.read_ram(0xA000), 0xFF);
    }

    #[test]
    fn plain_mbc3_keeps_four_ram_banks_and_seven_bit_rom_bank() {
        // 32 KiB RAM: selectors 4-7 are unmapped and must not alias banks 0-3.
        let mut c = from_rom(banked_rom(128, 0x13, 0x06, 0x03)).unwrap();
        c.write_rom(0x0000, 0x0A);
        c.write_rom(0x4000, 0);
        c.write_ram(0xA000, 0x42);
        for sel in 4..8u8 {
            c.write_rom(0x4000, sel);
            assert_eq!(c.read_ram(0xA000), 0xFF, "selector {sel}");
            c.write_ram(0xA000, 0x99);
        }
        c.write_rom(0x4000, 0);
        assert_eq!(c.read_ram(0xA000), 0x42);
        c.write_rom(0x2000, 0x80);
        assert_eq!(c.read_rom(0x4000), 1, "bit 7 ignored on MBC3");
    }

    #[test]
    fn rom_bank_is_masked_to_rom_size() {
        let mut c = from_rom(banked_rom(4, 0x11, 0x01, 0)).unwrap();
        c.write_rom(0x2000, 0x07);
        assert_eq!(c.read_rom(0x4000), 3);
    }

    #[test]
    fn ram_enable_and_four_banks() {
        let mut c = from_rom(banked_rom(2, 0x13, 0, 0x03)).unwrap();
        c.write_ram(0xA000, 0x42);
        assert_eq!(c.read_ram(0xA000), 0xFF, "disabled at power-on");
        c.write_rom(0x0000, 0x0A);
        for bank in 0..4u8 {
            c.write_rom(0x4000, bank);
            c.write_ram(0xA000, 0x10 + bank);
            c.write_ram(0xBFFF, 0x20 + bank);
        }
        for bank in 0..4u8 {
            c.write_rom(0x4000, bank);
            assert_eq!(c.read_ram(0xA000), 0x10 + bank);
            assert_eq!(c.read_ram(0xBFFF), 0x20 + bank);
        }
        c.write_rom(0x4000, 0x05); // unmapped selector
        assert_eq!(c.read_ram(0xA000), 0xFF);
        c.write_rom(0x4000, 0);
        c.write_rom(0x0000, 0x00);
        assert_eq!(c.read_ram(0xA000), 0xFF);
        c.write_rom(0x0000, 0x0A);
        assert_eq!(c.read_ram(0xA000), 0x10, "contents survive disable");
    }

    #[test]
    fn small_ram_wraps_instead_of_panicking() {
        let mut c = from_rom(banked_rom(2, 0x12, 0, 0x01)).unwrap(); // 2 KiB
        c.write_rom(0x0000, 0x0A);
        c.write_ram(0xA005, 0x77);
        assert_eq!(c.read_ram(0xA805), 0x77);
        c.write_rom(0x4000, 3);
        assert_eq!(c.read_ram(0xA005), 0x77);
    }

    #[test]
    fn rtc_registers_need_latch_and_enable() {
        let mut c = rtc_cart();
        tick_seconds(c.as_mut(), 75);
        assert_eq!(read_rtc(c.as_mut(), 0x08), 0, "not latched yet");
        latch(c.as_mut());
        assert_eq!(read_rtc(c.as_mut(), 0x08), 15);
        assert_eq!(read_rtc(c.as_mut(), 0x09), 1);
        assert_eq!(read_rtc(c.as_mut(), 0x0A), 0);
        tick_seconds(c.as_mut(), 10);
        assert_eq!(read_rtc(c.as_mut(), 0x08), 15, "latched value is frozen");
        latch(c.as_mut());
        assert_eq!(read_rtc(c.as_mut(), 0x08), 25);
        c.write_rom(0x0000, 0x00);
        assert_eq!(read_rtc(c.as_mut(), 0x08), 0xFF, "disabled");
    }

    #[test]
    fn latch_requires_zero_then_one() {
        let mut c = rtc_cart();
        tick_seconds(c.as_mut(), 5);
        c.write_rom(0x6000, 1);
        assert_eq!(read_rtc(c.as_mut(), 0x08), 0, "1 alone does not latch");
        c.write_rom(0x6000, 0);
        c.write_rom(0x7FFF, 2);
        c.write_rom(0x6000, 1);
        assert_eq!(read_rtc(c.as_mut(), 0x08), 0, "0,2,1 does not latch");
        c.write_rom(0x6000, 0);
        c.write_rom(0x7FFF, 1);
        assert_eq!(read_rtc(c.as_mut(), 0x08), 5);
        c.write_rom(0x7FFF, 1);
        tick_seconds(c.as_mut(), 1);
        c.write_rom(0x7FFF, 1);
        assert_eq!(read_rtc(c.as_mut(), 0x08), 5, "1,1 does not re-latch");
    }

    #[test]
    fn rtc_rolls_over_through_days_and_sets_carry() {
        let mut c = rtc_cart();
        c.write_rom(0x4000, 0x08);
        c.write_ram(0xA000, 59);
        c.write_rom(0x4000, 0x09);
        c.write_ram(0xA000, 59);
        c.write_rom(0x4000, 0x0A);
        c.write_ram(0xA000, 23);
        c.write_rom(0x4000, 0x0B);
        c.write_ram(0xA000, 0xFF);
        c.write_rom(0x4000, 0x0C);
        c.write_ram(0xA000, 0x01); // day 511
        tick_seconds(c.as_mut(), 1);
        latch(c.as_mut());
        assert_eq!(read_rtc(c.as_mut(), 0x08), 0);
        assert_eq!(read_rtc(c.as_mut(), 0x09), 0);
        assert_eq!(read_rtc(c.as_mut(), 0x0A), 0);
        assert_eq!(read_rtc(c.as_mut(), 0x0B), 0);
        assert_eq!(
            read_rtc(c.as_mut(), 0x0C),
            DH_CARRY,
            "day wrapped, carry set"
        );
        tick_seconds(c.as_mut(), 1);
        latch(c.as_mut());
        assert_eq!(read_rtc(c.as_mut(), 0x08), 1);
        assert_eq!(read_rtc(c.as_mut(), 0x0C), DH_CARRY, "carry is sticky");
    }

    #[test]
    fn halt_stops_the_clock_and_register_writes_are_masked() {
        let mut c = rtc_cart();
        c.write_rom(0x4000, 0x0C);
        c.write_ram(0xA000, DH_HALT | 0x3E); // junk bits dropped
        tick_seconds(c.as_mut(), 5);
        latch(c.as_mut());
        assert_eq!(read_rtc(c.as_mut(), 0x08), 0, "halted");
        assert_eq!(read_rtc(c.as_mut(), 0x0C), DH_HALT);
        c.write_rom(0x4000, 0x0C);
        c.write_ram(0xA000, 0);
        tick_seconds(c.as_mut(), 5);
        latch(c.as_mut());
        assert_eq!(read_rtc(c.as_mut(), 0x08), 5);
        c.write_rom(0x4000, 0x0A);
        c.write_ram(0xA000, 0xFF);
        latch(c.as_mut());
        assert_eq!(read_rtc(c.as_mut(), 0x0A), 0x1F);
    }

    #[test]
    fn writing_seconds_resets_the_prescaler() {
        let mut c = rtc_cart();
        tick_seconds(c.as_mut(), 1);
        for _ in 0..MCYCLES_PER_SECOND - 1 {
            c.tick_mcycle();
        }
        c.write_rom(0x4000, 0x08);
        c.write_ram(0xA000, 10);
        c.tick_mcycle();
        latch(c.as_mut());
        assert_eq!(read_rtc(c.as_mut(), 0x08), 10);
        for _ in 0..MCYCLES_PER_SECOND - 1 {
            c.tick_mcycle();
        }
        latch(c.as_mut());
        assert_eq!(read_rtc(c.as_mut(), 0x08), 11);
    }

    #[test]
    fn clock_is_exactly_2_pow_20_mcycles_per_second() {
        let mut c = rtc_cart();
        for _ in 0..MCYCLES_PER_SECOND - 1 {
            c.tick_mcycle();
        }
        latch(c.as_mut());
        assert_eq!(read_rtc(c.as_mut(), 0x08), 0);
        c.tick_mcycle();
        latch(c.as_mut());
        assert_eq!(read_rtc(c.as_mut(), 0x08), 1);
    }

    #[test]
    fn rtc_registers_absent_without_rtc() {
        let mut c = from_rom(banked_rom(2, 0x13, 0, 0x03)).unwrap();
        c.write_rom(0x0000, 0x0A);
        c.write_rom(0x4000, 0x08);
        assert_eq!(c.read_ram(0xA000), 0xFF);
        c.write_ram(0xA000, 5); // must not hit RAM
        c.write_rom(0x4000, 0x00);
        assert_eq!(c.read_ram(0xA000), 0x00);
    }

    #[test]
    fn save_round_trip_includes_ram_and_rtc() {
        let mut c = rtc_cart();
        c.write_rom(0x4000, 2);
        c.write_ram(0xA010, 0x5A);
        tick_seconds(c.as_mut(), 90);
        latch(c.as_mut());
        tick_seconds(c.as_mut(), 3);
        let save = c.battery_ram().unwrap();
        assert_eq!(save.len(), 0x8000 + RTC_FOOTER_LEN);

        let mut d = from_rom(banked_rom(4, 0x10, 0x01, 0x03)).unwrap();
        d.load_battery_ram(&save).unwrap();
        d.write_rom(0x0000, 0x0A);
        d.write_rom(0x4000, 2);
        assert_eq!(d.read_ram(0xA010), 0x5A);
        assert_eq!(read_rtc(d.as_mut(), 0x08), 30, "latched regs restored");
        latch(d.as_mut());
        assert_eq!(read_rtc(d.as_mut(), 0x08), 33, "live regs restored");
        assert_eq!(read_rtc(d.as_mut(), 0x09), 1);
        assert_eq!(d.battery_ram().unwrap().len(), save.len());
    }

    #[test]
    fn load_accepts_bare_ram_and_short_footer_but_rejects_other_sizes() {
        let mut c = rtc_cart();
        c.load_battery_ram(&vec![7; 0x8000]).unwrap();
        c.load_battery_ram(&vec![0; 0x8000 + RTC_FOOTER_LEN_SHORT])
            .unwrap();
        for len in [0, 100, 0x8000 + 1, 0x8000 + 47, 0x8000 + 49, 0x8000 - 1] {
            assert_eq!(
                c.load_battery_ram(&vec![0; len]),
                Err(CartridgeError::SaveSizeMismatch {
                    expected: 0x8000,
                    actual: len
                }),
                "len {len}"
            );
        }
        let mut n = from_rom(banked_rom(2, 0x12, 0, 0x03)).unwrap();
        assert_eq!(n.battery_ram(), None);
        assert_eq!(n.load_battery_ram(&[]), Err(CartridgeError::NoBattery));
    }

    #[test]
    fn garbage_rtc_footer_cannot_break_invariants() {
        let mut c = rtc_cart();
        c.load_battery_ram(&vec![0xFF; 0x8000 + RTC_FOOTER_LEN])
            .unwrap();
        tick_seconds(c.as_mut(), 2);
        latch(c.as_mut());
        assert!(read_rtc(c.as_mut(), 0x08) < 64);
        assert!(read_rtc(c.as_mut(), 0x0A) < 32);
    }

    #[test]
    fn rtc_only_cartridge_saves_just_the_footer() {
        let mut c = from_rom(banked_rom(2, 0x0F, 0, 0)).unwrap();
        let save = c.battery_ram().unwrap();
        assert_eq!(save.len(), RTC_FOOTER_LEN);
        c.load_battery_ram(&save).unwrap();
    }
}
