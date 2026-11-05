//! The Game Pak: ROM, header, and the save backend (SRAM / Flash / EEPROM). Owned by
//! P1-SYS (P3-CART adds GPIO/RTC and wider save detection).
//!
//! **Contract:** [`Cart::from_rom`], [`Cart::title`], [`CartError`], and the
//! region accessors the bus calls.

mod detect;
mod eeprom;
mod flash;

use std::fmt;

pub use detect::SaveKind;
use detect::detect_save;
use eeprom::Eeprom;
use flash::Flash;

/// Why a ROM image was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CartError {
    /// Shorter than the 192-byte header.
    TooSmall(usize),
    /// Larger than 32 MiB.
    TooLarge(usize),
    /// A save file whose size doesn't match the cartridge's backup chip.
    SaveSize { got: usize, expected: &'static str },
    /// A save file for a cartridge with no backup chip.
    NoSave,
}

impl fmt::Display for CartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooSmall(n) => write!(f, "ROM is {n} bytes, smaller than a GBA header"),
            Self::TooLarge(n) => write!(
                f,
                "ROM is {n} bytes, larger than the 32 MiB a GBA cartridge can map"
            ),
            Self::SaveSize { got, expected } => {
                write!(f, "save file is {got} bytes, expected {expected}")
            }
            Self::NoSave => write!(f, "this cartridge has no battery-backed save"),
        }
    }
}

impl std::error::Error for CartError {}

/// Size of the cartridge header (0x00..0xC0).
pub const HEADER_SIZE: usize = 0xC0;
/// SRAM size.
pub const SRAM_SIZE: usize = 32 * 1024;

enum Backup {
    None,
    Sram(Box<[u8]>),
    Flash(Flash),
    Eeprom(Eeprom),
}

pub struct Cart {
    rom: Vec<u8>,
    backup: Backup,
    /// Bumps on every change to the save data and on a successful load.
    generation: u64,
}

impl Cart {
    pub fn from_rom(rom: Vec<u8>) -> Result<Self, CartError> {
        if rom.len() < HEADER_SIZE {
            return Err(CartError::TooSmall(rom.len()));
        }
        if rom.len() > crate::MAX_ROM_SIZE {
            return Err(CartError::TooLarge(rom.len()));
        }
        let backup = match detect_save(&rom) {
            SaveKind::None => Backup::None,
            SaveKind::Sram => Backup::Sram(vec![0xFF; SRAM_SIZE].into_boxed_slice()),
            SaveKind::Flash64 => Backup::Flash(Flash::new(flash::SIZE_64K)),
            SaveKind::Flash128 => Backup::Flash(Flash::new(flash::SIZE_128K)),
            SaveKind::Eeprom => Backup::Eeprom(Eeprom::default()),
        };
        Ok(Self {
            rom,
            backup,
            generation: 0,
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

    /// ROM byte at `offset` (0..0x0200_0000). Beyond the image the cartridge bus
    /// floats and returns the address-derived value `(offset / 2) & 0xFFFF` per
    /// halfword (GBATEK "GBA Cart ROM"; DECISIONS #52).
    #[inline]
    pub fn read_rom8(&self, offset: u32) -> u8 {
        match self.rom.get(offset as usize) {
            Some(&b) => b,
            None => ((offset >> 1) as u16 >> ((offset & 1) * 8)) as u8,
        }
    }

    /// ROM halfword at the even `offset`.
    #[inline]
    pub fn read_rom16(&self, offset: u32) -> u16 {
        let o = offset as usize;
        match self.rom.get(o..o + 2) {
            Some(b) => u16::from_le_bytes([b[0], b[1]]),
            None => (offset >> 1) as u16,
        }
    }

    /// ROM word at the word-aligned `offset`.
    #[inline]
    pub fn read_rom32(&self, offset: u32) -> u32 {
        let o = offset as usize;
        match self.rom.get(o..o + 4) {
            Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            None => {
                u32::from(self.read_rom16(offset)) | u32::from(self.read_rom16(offset + 2)) << 16
            }
        }
    }

    /// Which backup chip was detected.
    pub fn save_kind(&self) -> SaveKind {
        match &self.backup {
            Backup::None => SaveKind::None,
            Backup::Sram(_) => SaveKind::Sram,
            Backup::Flash(f) if f.data().len() == flash::SIZE_128K => SaveKind::Flash128,
            Backup::Flash(_) => SaveKind::Flash64,
            Backup::Eeprom(_) => SaveKind::Eeprom,
        }
    }

    /// Byte from the SRAM/Flash window (0x0E00_0000..; `offset` is the low 16
    /// bits of the address). With no chip, or an EEPROM cartridge, reads 0xFF.
    #[inline]
    pub fn read_save8(&self, offset: u32) -> u8 {
        match &self.backup {
            Backup::Sram(d) => d[offset as usize & (SRAM_SIZE - 1)],
            Backup::Flash(f) => f.read(offset),
            Backup::None | Backup::Eeprom(_) => 0xFF,
        }
    }

    pub fn write_save8(&mut self, offset: u32, v: u8) {
        let changed = match &mut self.backup {
            Backup::Sram(d) => {
                let cell = &mut d[offset as usize & (SRAM_SIZE - 1)];
                let changed = *cell != v;
                *cell = v;
                changed
            }
            Backup::Flash(f) => f.write(offset, v),
            Backup::None | Backup::Eeprom(_) => false,
        };
        self.generation += u64::from(changed);
    }

    /// Whether `addr` (0x0800_0000..0x0E00_0000) reaches the EEPROM: the whole
    /// 0x0D00_0000 region for ROMs up to 16 MiB, otherwise only the top 256 bytes.
    #[inline]
    pub fn eeprom_at(&self, addr: u32) -> bool {
        matches!(self.backup, Backup::Eeprom(_))
            && addr >> 24 == 0x0D
            && (self.rom.len() <= 16 * 1024 * 1024 || addr >= 0x0DFF_FF00)
    }

    /// DMA3 is about to write `count` halfwords to the EEPROM.
    pub fn eeprom_begin_write_dma(&mut self, count: u32) {
        if let Backup::Eeprom(e) = &mut self.backup {
            e.begin_write_dma(count);
        }
    }

    /// One bit (bit 0 of a halfword) written to the EEPROM.
    pub fn eeprom_write(&mut self, v: u16) {
        if let Backup::Eeprom(e) = &mut self.backup {
            self.generation += u64::from(e.write_bit(v));
        }
    }

    /// One bit read from the EEPROM (1 when idle).
    pub fn eeprom_read(&mut self) -> u16 {
        match &mut self.backup {
            Backup::Eeprom(e) => e.read_bit(),
            _ => 1,
        }
    }

    /// The save data to persist (`None` without a backup chip, or for an EEPROM
    /// whose size isn't known yet because the game hasn't touched it).
    pub fn battery_ram(&self) -> Option<&[u8]> {
        match &self.backup {
            Backup::None => None,
            Backup::Sram(d) => Some(d),
            Backup::Flash(f) => Some(f.data()),
            Backup::Eeprom(e) => e.data(),
        }
    }

    /// Advances only when the save data changes (or a save is loaded); never
    /// for rewriting the same value. Cheap to poll, like `gb-core`'s.
    pub fn battery_generation(&self) -> u64 {
        self.generation
    }

    /// Restore a `.sav` image. The file is untrusted: its length must match the
    /// detected chip (EEPROM accepts 512 or 8192 and takes its size from it).
    pub fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), CartError> {
        let size_err = |expected| CartError::SaveSize {
            got: data.len(),
            expected,
        };
        match &mut self.backup {
            Backup::None => return Err(CartError::NoSave),
            Backup::Sram(d) => {
                if data.len() != SRAM_SIZE {
                    return Err(size_err("32768 (SRAM)"));
                }
                d.copy_from_slice(data);
            }
            Backup::Flash(f) => {
                if data.len() != f.data().len() {
                    return Err(size_err(if f.data().len() == flash::SIZE_128K {
                        "131072 (128 KiB flash)"
                    } else {
                        "65536 (64 KiB flash)"
                    }));
                }
                f.load(data);
            }
            Backup::Eeprom(e) => {
                if data.len() != eeprom::SIZE_512 && data.len() != eeprom::SIZE_8K {
                    return Err(size_err("512 or 8192 (EEPROM)"));
                }
                e.load(data);
            }
        }
        self.generation += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rom_with(id: &[u8]) -> Vec<u8> {
        let mut rom = vec![0; 0x200];
        rom[0x100..0x100 + id.len()].copy_from_slice(id);
        rom
    }

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

    #[test]
    fn rom_beyond_the_image_is_address_derived() {
        let c = Cart::from_rom(vec![0xAA; 0x200]).unwrap();
        assert_eq!(c.read_rom16(0x200), 0x100);
        assert_eq!(c.read_rom16(0x1_0000), 0x8000);
        assert_eq!(c.read_rom32(0x204), 0x0103_0102);
        assert_eq!(c.read_rom8(0x200), 0x00);
        assert_eq!(c.read_rom8(0x201), 0x01);
        assert_eq!(c.read_rom8(0x202), 0x01);
        assert_eq!(c.read_rom16(0x1FE), 0xAAAA);
        assert_eq!(c.read_rom32(0x1FC), 0xAAAA_AAAA);
    }

    #[test]
    fn no_save_reads_ff_and_rejects_loads() {
        let mut c = Cart::from_rom(vec![0; 0x200]).unwrap();
        assert_eq!(c.read_save8(0), 0xFF);
        c.write_save8(0, 1);
        assert_eq!(c.read_save8(0), 0xFF);
        assert!(c.battery_ram().is_none());
        assert_eq!(c.load_battery_ram(&[0; 10]), Err(CartError::NoSave));
        assert_eq!(c.battery_generation(), 0);
    }

    #[test]
    fn sram_mirrors_every_32k_and_tracks_generation() {
        let mut c = Cart::from_rom(rom_with(b"SRAM_V113")).unwrap();
        assert_eq!(c.save_kind(), SaveKind::Sram);
        assert_eq!(c.read_save8(0), 0xFF);
        c.write_save8(0x10, 7);
        assert_eq!(c.read_save8(0x8010), 7);
        let g = c.battery_generation();
        assert_eq!(g, 1);
        c.write_save8(0x10, 7);
        assert_eq!(c.battery_generation(), g);
        assert_eq!(c.battery_ram().unwrap().len(), SRAM_SIZE);
    }

    #[test]
    fn load_validates_the_size_per_chip() {
        let mut sram = Cart::from_rom(rom_with(b"SRAM_V113")).unwrap();
        assert!(matches!(
            sram.load_battery_ram(&[0; 100]),
            Err(CartError::SaveSize { got: 100, .. })
        ));
        assert_eq!(sram.battery_generation(), 0);
        let mut img = vec![0x11; SRAM_SIZE];
        img[5] = 9;
        sram.load_battery_ram(&img).unwrap();
        assert_eq!(sram.read_save8(5), 9);
        assert_eq!(sram.battery_generation(), 1);

        let mut f64k = Cart::from_rom(rom_with(b"FLASH_V126")).unwrap();
        assert!(f64k.load_battery_ram(&vec![0; flash::SIZE_128K]).is_err());
        f64k.load_battery_ram(&vec![0x5A; flash::SIZE_64K]).unwrap();
        assert_eq!(f64k.read_save8(0xFFFF), 0x5A);

        let mut f128 = Cart::from_rom(rom_with(b"FLASH1M_V103")).unwrap();
        assert_eq!(f128.save_kind(), SaveKind::Flash128);
        assert!(f128.load_battery_ram(&vec![0; flash::SIZE_64K]).is_err());
        f128.load_battery_ram(&vec![0; flash::SIZE_128K]).unwrap();

        let mut ee = Cart::from_rom(rom_with(b"EEPROM_V124")).unwrap();
        assert!(ee.battery_ram().is_none());
        assert!(ee.load_battery_ram(&[0; 1000]).is_err());
        ee.load_battery_ram(&[0x77; 512]).unwrap();
        assert_eq!(ee.battery_ram().unwrap().len(), 512);
        ee.load_battery_ram(&vec![0x77; 8192]).unwrap();
        assert_eq!(ee.battery_ram().unwrap().len(), 8192);
    }

    #[test]
    fn eeprom_decode_window_depends_on_rom_size() {
        let small = Cart::from_rom(rom_with(b"EEPROM_V124")).unwrap();
        assert!(small.eeprom_at(0x0D00_0000));
        assert!(small.eeprom_at(0x0DFF_FF00));
        assert!(!small.eeprom_at(0x0C00_0000));
        let mut big = rom_with(b"EEPROM_V124");
        big.resize(32 * 1024 * 1024, 0);
        let big = Cart::from_rom(big).unwrap();
        assert!(!big.eeprom_at(0x0D00_0000));
        assert!(big.eeprom_at(0x0DFF_FF00));
        let none = Cart::from_rom(vec![0; 0x200]).unwrap();
        assert!(!none.eeprom_at(0x0DFF_FF00));
    }

    #[test]
    fn flash_writes_bump_the_generation_only_when_data_changes() {
        let mut c = Cart::from_rom(rom_with(b"FLASH_V126")).unwrap();
        // Unlock + command bytes don't change data.
        for (a, v) in [(0x5555, 0xAA), (0x2AAA, 0x55), (0x5555, 0xA0)] {
            c.write_save8(a, v);
        }
        assert_eq!(c.battery_generation(), 0);
        c.write_save8(3, 0x12);
        assert_eq!(c.battery_generation(), 1);
        assert_eq!(c.read_save8(3), 0x12);
    }
}
