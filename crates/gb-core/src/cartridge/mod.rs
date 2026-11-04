//! Cartridges: header parsing, the [`Cartridge`] trait and MBC implementations.
//!
//! Pan Docs: "The Cartridge Header", "MBCs".

mod mbc1;
mod mbc3;
mod mbc5;
mod rom_only;

use std::fmt;

pub use mbc1::Mbc1;
pub use rom_only::RomOnly;

/// Size of one switchable ROM bank.
pub const ROM_BANK_SIZE: usize = 0x4000;
/// Size of one switchable external RAM bank.
pub const RAM_BANK_SIZE: usize = 0x2000;

/// The memory-bank controller interface every cartridge implements.
///
/// # Contract (frozen; propose changes to the orchestrator)
///
/// * `read_rom`/`write_rom` receive CPU addresses `0x0000..=0x7FFF`.
///   Writes to this range are MBC register writes.
/// * `read_ram`/`write_ram` receive CPU addresses `0xA000..=0xBFFF`.
///   Disabled or absent RAM reads `0xFF` and ignores writes.
/// * `tick_mcycle` is called once per M-cycle (for MBC3 RTC); default no-op.
///   It must not read the host clock: RTC time advances with emulated time only.
/// * Battery RAM: [`Cartridge::battery_ram`] returns the bytes to persist (only
///   if the cartridge has a battery); [`Cartridge::load_battery_ram`] restores them.
pub trait Cartridge: Send {
    fn header(&self) -> &CartridgeHeader;
    fn read_rom(&self, addr: u16) -> u8;
    fn write_rom(&mut self, addr: u16, value: u8);
    fn read_ram(&self, addr: u16) -> u8;
    fn write_ram(&mut self, addr: u16, value: u8);
    fn tick_mcycle(&mut self) {}

    /// Persistent RAM contents, or `None` if the cartridge has no battery.
    fn battery_ram(&self) -> Option<Vec<u8>>;

    /// Restore persistent RAM from a `.sav` file.
    fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError>;
}

/// Errors while loading a ROM or save file. All ROM content is untrusted input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CartridgeError {
    /// The file is smaller than the 0x150-byte header area.
    TooSmall(usize),
    /// The file is larger than the largest DMG mapper supports (8 MiB).
    TooLarge(usize),
    /// Cartridge type byte (0x0147) not supported.
    UnsupportedType(u8),
    /// ROM size byte (0x0148) not valid.
    InvalidRomSize(u8),
    /// RAM size byte (0x0149) not valid.
    InvalidRamSize(u8),
    /// Save file size does not match the cartridge RAM size.
    SaveSizeMismatch { expected: usize, actual: usize },
    /// The cartridge has no battery-backed RAM.
    NoBattery,
}

impl fmt::Display for CartridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooSmall(n) => write!(f, "ROM too small ({n} bytes, need at least 0x150)"),
            Self::TooLarge(n) => write!(f, "ROM too large ({n} bytes, max 8 MiB)"),
            Self::UnsupportedType(t) => write!(f, "unsupported cartridge type 0x{t:02X}"),
            Self::InvalidRomSize(s) => write!(f, "invalid ROM size code 0x{s:02X}"),
            Self::InvalidRamSize(s) => write!(f, "invalid RAM size code 0x{s:02X}"),
            Self::SaveSizeMismatch { expected, actual } => {
                write!(
                    f,
                    "save file is {actual} bytes, cartridge RAM is {expected}"
                )
            }
            Self::NoBattery => write!(f, "cartridge has no battery-backed RAM"),
        }
    }
}

impl std::error::Error for CartridgeError {}

/// Which MBC a cartridge type byte selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MbcKind {
    RomOnly,
    Mbc1,
    Mbc3,
    Mbc5,
}

/// Parsed cartridge header (0x0100..=0x014F).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CartridgeHeader {
    /// Title (0x0134..=0x0143), trailing NULs stripped, non-ASCII replaced.
    pub title: String,
    pub cart_type: u8,
    pub mbc: MbcKind,
    pub has_ram: bool,
    pub has_battery: bool,
    pub has_rtc: bool,
    pub has_rumble: bool,
    /// Declared ROM size in bytes.
    pub rom_size: usize,
    /// Declared external RAM size in bytes (0 if none).
    pub ram_size: usize,
    /// Header checksum (0x014D) matches the computed one.
    pub header_checksum_ok: bool,
    /// CGB flag (0x0143). Kept for future CGB support; DMG ignores it.
    pub cgb_flag: u8,
}

impl CartridgeHeader {
    pub fn parse(rom: &[u8]) -> Result<Self, CartridgeError> {
        if rom.len() < 0x150 {
            return Err(CartridgeError::TooSmall(rom.len()));
        }
        let cart_type = rom[0x147];
        // (mbc, ram, battery, rtc, rumble)
        let (mbc, has_ram, has_battery, has_rtc, has_rumble) = match cart_type {
            0x00 => (MbcKind::RomOnly, false, false, false, false),
            0x08 => (MbcKind::RomOnly, true, false, false, false),
            0x09 => (MbcKind::RomOnly, true, true, false, false),
            0x01 => (MbcKind::Mbc1, false, false, false, false),
            0x02 => (MbcKind::Mbc1, true, false, false, false),
            0x03 => (MbcKind::Mbc1, true, true, false, false),
            0x0F => (MbcKind::Mbc3, false, true, true, false),
            0x10 => (MbcKind::Mbc3, true, true, true, false),
            0x11 => (MbcKind::Mbc3, false, false, false, false),
            0x12 => (MbcKind::Mbc3, true, false, false, false),
            0x13 => (MbcKind::Mbc3, true, true, false, false),
            0x19 => (MbcKind::Mbc5, false, false, false, false),
            0x1A => (MbcKind::Mbc5, true, false, false, false),
            0x1B => (MbcKind::Mbc5, true, true, false, false),
            0x1C => (MbcKind::Mbc5, false, false, false, true),
            0x1D => (MbcKind::Mbc5, true, false, false, true),
            0x1E => (MbcKind::Mbc5, true, true, false, true),
            t => return Err(CartridgeError::UnsupportedType(t)),
        };
        let rom_code = rom[0x148];
        if rom_code > 0x08 {
            return Err(CartridgeError::InvalidRomSize(rom_code));
        }
        let rom_size = 0x8000usize << rom_code;
        let ram_code = rom[0x149];
        let ram_size = match ram_code {
            0x00 => 0,
            0x01 => 0x800, // unofficial 2 KiB
            0x02 => 0x2000,
            0x03 => 0x8000,
            0x04 => 0x20000,
            0x05 => 0x10000,
            c => return Err(CartridgeError::InvalidRamSize(c)),
        };
        let checksum = rom[0x134..=0x14C]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_sub(b).wrapping_sub(1));
        let title = rom[0x134..0x144]
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '?'
                }
            })
            .collect::<String>()
            .trim_end()
            .to_string();
        Ok(Self {
            title,
            cart_type,
            mbc,
            has_ram,
            has_battery,
            has_rtc,
            has_rumble,
            rom_size,
            ram_size: if has_ram { ram_size } else { 0 },
            header_checksum_ok: checksum == rom[0x14D],
            cgb_flag: rom[0x143],
        })
    }
}

/// Largest ROM any supported DMG mapper can address (MBC5: 512 banks).
pub const MAX_ROM_SIZE: usize = 8 * 1024 * 1024;

/// Pad ROM data to a power-of-two number of 16 KiB banks (at least 2) and at
/// least the declared size, so bank masking never indexes out of bounds even
/// for truncated or oversized-header ROMs.
pub(crate) fn normalize_rom(mut rom: Vec<u8>, declared: usize) -> Vec<u8> {
    let banks = rom
        .len()
        .max(declared)
        .div_ceil(ROM_BANK_SIZE)
        .max(2)
        .next_power_of_two();
    rom.resize(banks * ROM_BANK_SIZE, 0xFF);
    rom
}

/// Parse the header and construct the matching cartridge.
pub fn from_rom(rom: Vec<u8>) -> Result<Box<dyn Cartridge>, CartridgeError> {
    if rom.len() > MAX_ROM_SIZE {
        return Err(CartridgeError::TooLarge(rom.len()));
    }
    let header = CartridgeHeader::parse(&rom)?;
    let rom = normalize_rom(rom, header.rom_size);
    Ok(match header.mbc {
        MbcKind::RomOnly => Box::new(RomOnly::new(rom, header)),
        MbcKind::Mbc1 => Box::new(Mbc1::new(rom, header)),
        MbcKind::Mbc3 | MbcKind::Mbc5 => {
            return Err(CartridgeError::UnsupportedType(header.cart_type));
        }
    })
}

/// Shared battery-RAM helpers for MBC implementations.
pub(crate) fn battery_ram(header: &CartridgeHeader, ram: &[u8]) -> Option<Vec<u8>> {
    (header.has_battery && !ram.is_empty()).then(|| ram.to_vec())
}

pub(crate) fn load_battery_ram(
    header: &CartridgeHeader,
    ram: &mut [u8],
    data: &[u8],
) -> Result<(), CartridgeError> {
    if !header.has_battery {
        return Err(CartridgeError::NoBattery);
    }
    if data.len() != ram.len() {
        return Err(CartridgeError::SaveSizeMismatch {
            expected: ram.len(),
            actual: data.len(),
        });
    }
    ram.copy_from_slice(data);
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_util {
    /// Build a ROM of `banks` 16 KiB banks where every byte holds its bank number,
    /// with a valid header for `cart_type`/`rom_code`/`ram_code`.
    pub fn banked_rom(banks: usize, cart_type: u8, rom_code: u8, ram_code: u8) -> Vec<u8> {
        let mut rom = vec![0u8; banks * super::ROM_BANK_SIZE];
        for (i, b) in rom.iter_mut().enumerate() {
            *b = (i / super::ROM_BANK_SIZE) as u8;
        }
        rom[0x134..0x144].fill(0);
        rom[0x134..0x138].copy_from_slice(b"TEST");
        rom[0x147] = cart_type;
        rom[0x148] = rom_code;
        rom[0x149] = ram_code;
        rom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_tiny_rom() {
        assert_eq!(
            CartridgeHeader::parse(&[0; 0x100]),
            Err(CartridgeError::TooSmall(0x100))
        );
    }

    #[test]
    fn rejects_unknown_type() {
        let rom = test_util::banked_rom(2, 0xFC, 0, 0);
        assert!(matches!(
            from_rom(rom),
            Err(CartridgeError::UnsupportedType(0xFC))
        ));
    }

    #[test]
    fn parses_header_fields() {
        let rom = test_util::banked_rom(4, 0x03, 0x01, 0x03);
        let h = CartridgeHeader::parse(&rom).unwrap();
        assert_eq!(h.title, "TEST");
        assert_eq!(h.mbc, MbcKind::Mbc1);
        assert!(h.has_ram && h.has_battery && !h.has_rtc);
        assert_eq!(h.rom_size, 0x10000);
        assert_eq!(h.ram_size, 0x8000);
    }

    #[test]
    fn truncated_rom_is_padded() {
        let rom = normalize_rom(vec![0; 0x150], 0x8000);
        assert_eq!(rom.len(), 0x8000);
        let rom = normalize_rom(vec![0; 3 * ROM_BANK_SIZE], 0x8000);
        assert_eq!(rom.len(), 4 * ROM_BANK_SIZE);
    }
}
