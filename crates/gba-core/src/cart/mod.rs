//! The Game Pak: ROM, header, and the save backend (SRAM / Flash / EEPROM). Owned by
//! P1-SYS (P3-CART adds GPIO/RTC and wider save detection).
//!
//! **Contract:** [`Cart::from_rom`], [`Cart::title`], [`CartError`], and the
//! region accessors the bus calls.

use std::fmt;

/// Why a ROM image was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CartError {
    /// Shorter than the 192-byte header.
    TooSmall(usize),
    /// Larger than 32 MiB.
    TooLarge(usize),
}

impl fmt::Display for CartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooSmall(n) => write!(f, "ROM is {n} bytes, smaller than a GBA header"),
            Self::TooLarge(n) => write!(
                f,
                "ROM is {n} bytes, larger than the 32 MiB a GBA cartridge can map"
            ),
        }
    }
}

impl std::error::Error for CartError {}

/// Size of the cartridge header (0x00..0xC0).
pub const HEADER_SIZE: usize = 0xC0;

pub struct Cart {
    rom: Vec<u8>,
    /// Placeholder 64 KiB SRAM until P1-SYS adds the real backends.
    sram: Vec<u8>,
}

impl Cart {
    pub fn from_rom(rom: Vec<u8>) -> Result<Self, CartError> {
        if rom.len() < HEADER_SIZE {
            return Err(CartError::TooSmall(rom.len()));
        }
        if rom.len() > crate::MAX_ROM_SIZE {
            return Err(CartError::TooLarge(rom.len()));
        }
        Ok(Self {
            rom,
            sram: vec![0xFF; 0x1_0000],
        })
    }

    /// The 12-byte game title at 0xA0, trimmed at the first NUL.
    pub fn title(&self) -> String {
        let raw = &self.rom[0xA0..0xAC];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        raw[..end]
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '?'
                }
            })
            .collect()
    }

    pub fn rom(&self) -> &[u8] {
        &self.rom
    }

    /// ROM byte at `offset` (0..0x0200_0000). Beyond the image, the bus returns
    /// the address-derived open-bus value; P1-SYS implements that.
    pub fn read_rom8(&self, offset: u32) -> u8 {
        self.rom.get(offset as usize).copied().unwrap_or(0)
    }

    pub fn read_save8(&self, offset: u32) -> u8 {
        self.sram[offset as usize & 0xFFFF]
    }

    pub fn write_save8(&mut self, offset: u32, v: u8) {
        self.sram[offset as usize & 0xFFFF] = v;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_sizes_and_reads_title() {
        assert_eq!(
            Cart::from_rom(vec![0; 10]).err(),
            Some(CartError::TooSmall(10))
        );
        let mut rom = vec![0; HEADER_SIZE];
        rom[0xA0..0xA4].copy_from_slice(b"TEST");
        assert_eq!(Cart::from_rom(rom).unwrap().title(), "TEST");
    }
}
