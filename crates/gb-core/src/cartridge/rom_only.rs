//! No MBC ("MBC0"): 32 KiB ROM, optional 8 KiB RAM (types 0x00, 0x08, 0x09).

use super::{Cartridge, CartridgeError, CartridgeHeader, ram_window_offset};
use crate::state::{StateError, StateReader, StateWriter};

pub struct RomOnly {
    rom: Vec<u8>,
    ram: Vec<u8>,
    header: CartridgeHeader,
    battery_gen: u64,
}

impl RomOnly {
    pub fn new(rom: Vec<u8>, header: CartridgeHeader) -> Self {
        let ram = vec![0; header.ram_size.min(0x2000)];
        Self {
            rom,
            ram,
            header,
            battery_gen: 0,
        }
    }
}

impl Cartridge for RomOnly {
    fn header(&self) -> &CartridgeHeader {
        &self.header
    }

    fn read_rom(&self, addr: u16) -> u8 {
        self.rom[addr as usize & 0x7FFF]
    }

    fn write_rom(&mut self, _addr: u16, _value: u8) {}

    fn read_ram(&self, addr: u16) -> u8 {
        if self.ram.is_empty() {
            return 0xFF;
        }
        self.ram[ram_window_offset(addr) % self.ram.len()]
    }

    fn write_ram(&mut self, addr: u16, value: u8) {
        if !self.ram.is_empty() {
            let o = ram_window_offset(addr) % self.ram.len();
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
    }

    fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.blob_into(&mut self.ram, "cart RAM size")?;
        self.battery_gen += 1;
        Ok(())
    }
}
