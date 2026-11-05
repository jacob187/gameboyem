//! MBC5 (Pan Docs: "MBC5"). Up to 8 MiB ROM (512 banks) / 128 KiB RAM (16 banks),
//! optional rumble motor.
//!
//! Registers:
//! * `0000-1FFF` RAM enable: any value with low nibble `0xA` enables, anything
//!   else disables (Pan Docs: real MBCs decode only the low 4 bits, as MBC1/MBC3).
//! * `2000-2FFF` ROM bank, low 8 bits.
//! * `3000-3FFF` ROM bank, bit 8 (only bit 0 of the written value is used).
//! * `4000-5FFF` RAM bank, 4 bits. On rumble cartridges bit 3 drives the motor
//!   instead and is not part of the RAM bank number (the motor is not modelled).
//!
//! Unlike MBC1, bank 0 is a valid selection for `4000-7FFF`; there is no 0 -> 1
//! remapping. The ROM bank number is masked to the actual ROM size.

use super::{
    Cartridge, CartridgeError, CartridgeHeader, RAM_BANK_SIZE, ROM_BANK_SIZE, ram_window_offset,
};
use crate::state::{StateError, StateReader, StateWriter, check};

pub struct Mbc5 {
    rom: Vec<u8>,
    ram: Vec<u8>,
    header: CartridgeHeader,
    /// See [`Cartridge::battery_generation`].
    battery_gen: u64,
    rom_bank_mask: usize,
    ram_enabled: bool,
    /// 9-bit ROM bank number.
    rom_bank: u16,
    ram_bank: u8,
}

impl Mbc5 {
    pub fn new(rom: Vec<u8>, header: CartridgeHeader) -> Self {
        let ram = vec![0; header.ram_size];
        let rom_bank_mask = rom.len() / ROM_BANK_SIZE - 1;
        Self {
            rom,
            ram,
            header,
            battery_gen: 0,
            rom_bank_mask,
            ram_enabled: false,
            rom_bank: 1,
            ram_bank: 0,
        }
    }

    fn ram_offset(&self, addr: u16) -> Option<usize> {
        if !self.ram_enabled || self.ram.is_empty() {
            return None;
        }
        let offset = self.ram_bank as usize * RAM_BANK_SIZE + ram_window_offset(addr);
        Some(offset % self.ram.len())
    }
}

impl Cartridge for Mbc5 {
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
            0x2000..=0x2FFF => self.rom_bank = (self.rom_bank & 0x100) | value as u16,
            0x3000..=0x3FFF => self.rom_bank = (self.rom_bank & 0x0FF) | ((value as u16 & 1) << 8),
            0x4000..=0x5FFF => {
                if self.header.has_rumble {
                    // Bit 3 drives the motor; there is no output to emulate.
                    self.ram_bank = value & 0x07;
                } else {
                    self.ram_bank = value & 0x0F;
                }
            }
            _ => {}
        }
    }

    fn read_ram(&self, addr: u16) -> u8 {
        self.ram_offset(addr).map_or(0xFF, |o| self.ram[o])
    }

    fn write_ram(&mut self, addr: u16, value: u8) {
        if let Some(o) = self.ram_offset(addr) {
            if self.ram[o] != value {
                self.ram[o] = value;
                self.battery_gen += 1;
            }
        }
    }

    fn battery_ram(&self) -> Option<Vec<u8>> {
        super::battery_ram(&self.header, &self.ram)
    }

    fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        super::load_battery_ram(&self.header, &mut self.ram, data)?;
        self.battery_gen += 1;
        Ok(())
    }

    fn battery_generation(&self) -> u64 {
        self.battery_gen
    }

    fn save_state(&self, w: &mut StateWriter) {
        w.blob(&self.ram);
        w.bool(self.ram_enabled);
        w.u16(self.rom_bank);
        w.u8(self.ram_bank);
    }

    fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.blob_into(&mut self.ram, "cart RAM size")?;
        self.ram_enabled = r.bool()?;
        self.rom_bank = r.u16()?;
        check(self.rom_bank <= 0x1FF, "MBC5 ROM bank")?;
        self.ram_bank = r.u8()?;
        let max_bank = if self.header.has_rumble { 0x07 } else { 0x0F };
        check(self.ram_bank <= max_bank, "MBC5 RAM bank")?;
        self.battery_gen += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{from_rom, test_util::banked_rom};
    use super::*;

    /// ROM whose bank `n` starts with `n` as a 16-bit value (banked_rom only
    /// stores `n as u8`, which aliases above 256 banks).
    fn rom_512_banks(cart_type: u8, ram_code: u8) -> Vec<u8> {
        let mut rom = banked_rom(512, cart_type, 0x08, ram_code);
        for bank in 0..512usize {
            let o = bank * ROM_BANK_SIZE;
            // Keep the header bytes of bank 0 intact.
            if bank != 0 {
                rom[o] = (bank >> 8) as u8;
                rom[o + 1] = bank as u8;
            }
        }
        rom
    }

    fn bank_at_4000(c: &dyn Cartridge) -> usize {
        ((c.read_rom(0x4000) as usize) << 8) | c.read_rom(0x4001) as usize
    }

    #[test]
    fn bank_zero_is_valid_and_default_is_one() {
        let mut c = from_rom(banked_rom(4, 0x19, 0x01, 0)).unwrap();
        assert_eq!(c.read_rom(0x4000), 1, "power-on bank 1");
        c.write_rom(0x2000, 0);
        assert_eq!(c.read_rom(0x4000), 0, "bank 0 is selectable, no 0->1 remap");
        assert_eq!(c.read_rom(0x0000), 0);
        c.write_rom(0x2000, 3);
        assert_eq!(c.read_rom(0x4000), 3);
    }

    #[test]
    fn nine_bit_rom_bank() {
        let mut c = from_rom(rom_512_banks(0x19, 0)).unwrap();
        assert_eq!(c.header().rom_size, 8 * 1024 * 1024);
        for bank in [0usize, 1, 2, 0x7F, 0x80, 0xFF, 0x100, 0x101, 0x1AB, 0x1FF] {
            c.write_rom(0x2000, bank as u8);
            c.write_rom(0x3000, (bank >> 8) as u8);
            let got = bank_at_4000(c.as_ref());
            if bank == 0 {
                // Bank 0 starts with the real header bytes, not our marker.
                assert_eq!(c.read_rom(0x4000), c.read_rom(0x0000));
            } else {
                assert_eq!(got, bank, "bank {bank:#x}");
            }
        }
    }

    #[test]
    fn bank_bit8_uses_only_bit0_and_registers_are_independent() {
        let mut c = from_rom(rom_512_banks(0x19, 0)).unwrap();
        c.write_rom(0x2000, 0x34);
        c.write_rom(0x3000, 0xFF); // only bit 0 counts
        assert_eq!(bank_at_4000(c.as_ref()), 0x134);
        c.write_rom(0x3000, 0xFE);
        assert_eq!(bank_at_4000(c.as_ref()), 0x034);
        c.write_rom(0x2FFF, 0x05); // anywhere in 2000-2FFF
        assert_eq!(bank_at_4000(c.as_ref()), 0x005);
        c.write_rom(0x3FFF, 0x01); // anywhere in 3000-3FFF
        assert_eq!(bank_at_4000(c.as_ref()), 0x105);
    }

    #[test]
    fn rom_bank_is_masked_to_rom_size() {
        let mut c = from_rom(banked_rom(4, 0x19, 0x01, 0)).unwrap();
        c.write_rom(0x2000, 0x07);
        assert_eq!(c.read_rom(0x4000), 3);
        c.write_rom(0x3000, 1);
        c.write_rom(0x2000, 0x02);
        assert_eq!(c.read_rom(0x4000), 2, "bit 8 masked away on a 64 KiB ROM");
    }

    #[test]
    fn ram_enable_decodes_low_nibble() {
        let mut c = from_rom(banked_rom(2, 0x1A, 0, 0x02)).unwrap();
        c.write_ram(0xA000, 0x42);
        assert_eq!(c.read_ram(0xA000), 0xFF, "disabled at power-on");
        c.write_rom(0x0000, 0x0A);
        c.write_ram(0xA000, 0x42);
        assert_eq!(c.read_ram(0xA000), 0x42);
        c.write_rom(0x0000, 0x00);
        assert_eq!(c.read_ram(0xA000), 0xFF);
        for v in [0x1A, 0x2A, 0xFA] {
            c.write_rom(0x1FFF, v); // low nibble 0xA enables
            assert_eq!(
                c.read_ram(0xA000),
                0x42,
                "{v:#04x} enables; contents survive"
            );
            c.write_rom(0x0000, 0x00);
        }
        for v in [0x0B, 0x0F, 0xA0, 0x01] {
            c.write_rom(0x0000, 0x0A);
            c.write_rom(0x1000, v);
            assert_eq!(c.read_ram(0xA000), 0xFF, "{v:#04x} disables");
        }
    }

    #[test]
    fn sixteen_ram_banks() {
        // RAM code 4 = 128 KiB = 16 banks.
        let mut c = from_rom(banked_rom(2, 0x1B, 0, 0x04)).unwrap();
        c.write_rom(0x0000, 0x0A);
        for bank in 0..16u8 {
            c.write_rom(0x4000, bank);
            c.write_ram(0xA000, 0x10 + bank);
            c.write_ram(0xBFFF, 0x20 + bank);
        }
        for bank in 0..16u8 {
            c.write_rom(0x4000, bank);
            assert_eq!(c.read_ram(0xA000), 0x10 + bank);
            assert_eq!(c.read_ram(0xBFFF), 0x20 + bank);
        }
        // Upper nibble bits of the RAM bank register are ignored.
        c.write_rom(0x4000, 0xF3);
        assert_eq!(c.read_ram(0xA000), 0x10 + 3);
        let save = c.battery_ram().unwrap();
        assert_eq!(save.len(), 0x20000);
        assert_eq!(save[5 * 0x2000], 0x15);
    }

    #[test]
    fn small_ram_wraps_instead_of_panicking() {
        let mut c = from_rom(banked_rom(2, 0x1A, 0, 0x02)).unwrap(); // 8 KiB
        c.write_rom(0x0000, 0x0A);
        c.write_rom(0x4000, 0x00);
        c.write_ram(0xA123, 0x77);
        c.write_rom(0x4000, 0x05);
        assert_eq!(c.read_ram(0xA123), 0x77, "single bank aliases");
    }

    #[test]
    fn rumble_bit_is_not_part_of_ram_bank() {
        // 0x1E = MBC5 + RAM + battery + rumble.
        let mut c = from_rom(banked_rom(2, 0x1E, 0, 0x04)).unwrap();
        c.write_rom(0x0000, 0x0A);
        c.write_rom(0x4000, 0x02);
        c.write_ram(0xA000, 0xAB);
        c.write_rom(0x4000, 0x0A); // bank 2 + rumble motor on
        assert_eq!(c.read_ram(0xA000), 0xAB, "same RAM bank with rumble set");
        c.write_rom(0x4000, 0x03);
        assert_eq!(c.read_ram(0xA000), 0x00);
        // Without rumble, bit 3 is a RAM bank bit.
        let mut c = from_rom(banked_rom(2, 0x1B, 0, 0x04)).unwrap();
        c.write_rom(0x0000, 0x0A);
        c.write_rom(0x4000, 0x02);
        c.write_ram(0xA000, 0xAB);
        c.write_rom(0x4000, 0x0A);
        assert_eq!(c.read_ram(0xA000), 0x00);
    }

    #[test]
    fn no_ram_and_no_battery() {
        let mut c = from_rom(banked_rom(2, 0x19, 0, 0)).unwrap();
        c.write_rom(0x0000, 0x0A);
        c.write_ram(0xA000, 1);
        assert_eq!(c.read_ram(0xA000), 0xFF);
        assert_eq!(c.battery_ram(), None);
        assert_eq!(c.load_battery_ram(&[]), Err(CartridgeError::NoBattery));
    }

    #[test]
    fn save_round_trip_and_size_check() {
        let mut c = from_rom(banked_rom(2, 0x1B, 0, 0x03)).unwrap(); // 32 KiB
        c.write_rom(0x0000, 0x0A);
        c.write_rom(0x4000, 3);
        c.write_ram(0xA010, 0x5A);
        let save = c.battery_ram().unwrap();
        assert_eq!(save.len(), 0x8000);

        let mut d = from_rom(banked_rom(2, 0x1B, 0, 0x03)).unwrap();
        d.load_battery_ram(&save).unwrap();
        d.write_rom(0x0000, 0x0A);
        d.write_rom(0x4000, 3);
        assert_eq!(d.read_ram(0xA010), 0x5A);
        assert_eq!(
            d.load_battery_ram(&save[..100]),
            Err(CartridgeError::SaveSizeMismatch {
                expected: 0x8000,
                actual: 100
            })
        );
    }
}
