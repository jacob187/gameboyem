//! MBC1 (Pan Docs: "MBC1"). Up to 2 MiB ROM / 32 KiB RAM.
//!
//! Registers:
//! * `0000-1FFF` RAM enable (`0x0A` in the low nibble enables).
//! * `2000-3FFF` BANK1: 5-bit ROM bank; a written 0 is treated as 1.
//! * `4000-5FFF` BANK2: 2 bits, RAM bank or ROM bank bits 5-6.
//! * `6000-7FFF` mode: 0 = BANK2 only affects 4000-7FFF; 1 = BANK2 also
//!   selects the 0000-3FFF ROM bank and the RAM bank.
//!
//! MBC1M multicarts (different BANK2 wiring) are not detected.

use super::{Cartridge, CartridgeError, CartridgeHeader, RAM_BANK_SIZE, ROM_BANK_SIZE};

pub struct Mbc1 {
    rom: Vec<u8>,
    ram: Vec<u8>,
    header: CartridgeHeader,
    rom_bank_mask: usize,
    ram_enabled: bool,
    bank1: u8,
    bank2: u8,
    mode: bool,
}

impl Mbc1 {
    pub fn new(rom: Vec<u8>, header: CartridgeHeader) -> Self {
        let ram = vec![0; header.ram_size];
        let rom_bank_mask = rom.len() / ROM_BANK_SIZE - 1;
        Self {
            rom,
            ram,
            header,
            rom_bank_mask,
            ram_enabled: false,
            bank1: 1,
            bank2: 0,
            mode: false,
        }
    }

    fn low_bank(&self) -> usize {
        let bank = if self.mode {
            (self.bank2 as usize) << 5
        } else {
            0
        };
        bank & self.rom_bank_mask
    }

    fn high_bank(&self) -> usize {
        (((self.bank2 as usize) << 5) | self.bank1 as usize) & self.rom_bank_mask
    }

    fn ram_offset(&self, addr: u16) -> Option<usize> {
        if !self.ram_enabled || self.ram.is_empty() {
            return None;
        }
        let bank = if self.mode && self.ram.len() > RAM_BANK_SIZE {
            self.bank2 as usize
        } else {
            0
        };
        Some((bank * RAM_BANK_SIZE + (addr as usize - 0xA000)) % self.ram.len())
    }
}

impl Cartridge for Mbc1 {
    fn header(&self) -> &CartridgeHeader {
        &self.header
    }

    fn read_rom(&self, addr: u16) -> u8 {
        let offset = addr as usize & 0x3FFF;
        let bank = if addr < 0x4000 {
            self.low_bank()
        } else {
            self.high_bank()
        };
        self.rom[bank * ROM_BANK_SIZE + offset]
    }

    fn write_rom(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000..=0x1FFF => self.ram_enabled = value & 0x0F == 0x0A,
            0x2000..=0x3FFF => {
                let v = value & 0x1F;
                self.bank1 = if v == 0 { 1 } else { v };
            }
            0x4000..=0x5FFF => self.bank2 = value & 0x03,
            _ => self.mode = value & 0x01 != 0,
        }
    }

    fn read_ram(&self, addr: u16) -> u8 {
        self.ram_offset(addr).map_or(0xFF, |o| self.ram[o])
    }

    fn write_ram(&mut self, addr: u16, value: u8) {
        if let Some(o) = self.ram_offset(addr) {
            self.ram[o] = value;
        }
    }

    fn battery_ram(&self) -> Option<Vec<u8>> {
        super::battery_ram(&self.header, &self.ram)
    }

    fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        super::load_battery_ram(&self.header, &mut self.ram, data)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{from_rom, test_util::banked_rom};

    #[test]
    fn bank_zero_maps_to_one() {
        let mut c = from_rom(banked_rom(8, 0x01, 0x02, 0)).unwrap();
        assert_eq!(c.read_rom(0x4000), 1);
        c.write_rom(0x2000, 0);
        assert_eq!(c.read_rom(0x4000), 1);
        c.write_rom(0x2000, 5);
        assert_eq!(c.read_rom(0x4000), 5);
        assert_eq!(c.read_rom(0x0000), 0);
    }

    #[test]
    fn bank_number_is_masked_to_rom_size() {
        let mut c = from_rom(banked_rom(4, 0x01, 0x01, 0)).unwrap();
        c.write_rom(0x2000, 0x07);
        assert_eq!(c.read_rom(0x4000), 3);
    }

    #[test]
    fn upper_bits_and_mode1() {
        // 1 MiB = 64 banks.
        let mut c = from_rom(banked_rom(64, 0x01, 0x05, 0)).unwrap();
        c.write_rom(0x4000, 1);
        c.write_rom(0x2000, 0); // -> 1
        assert_eq!(c.read_rom(0x4000), 0x21);
        assert_eq!(c.read_rom(0x0000), 0);
        c.write_rom(0x6000, 1);
        assert_eq!(c.read_rom(0x0000), 0x20);
    }

    #[test]
    fn ram_enable_and_banking() {
        let mut c = from_rom(banked_rom(4, 0x03, 0x01, 0x03)).unwrap();
        assert_eq!(c.read_ram(0xA000), 0xFF);
        c.write_ram(0xA000, 0x12);
        c.write_rom(0x0000, 0x0A);
        assert_eq!(c.read_ram(0xA000), 0x00);
        c.write_ram(0xA000, 0x12);
        c.write_rom(0x6000, 1);
        c.write_rom(0x4000, 2);
        c.write_ram(0xA000, 0x34);
        c.write_rom(0x4000, 0);
        assert_eq!(c.read_ram(0xA000), 0x12);
        let save = c.battery_ram().unwrap();
        assert_eq!(save.len(), 0x8000);
        assert_eq!(save[2 * 0x2000], 0x34);
    }
}
