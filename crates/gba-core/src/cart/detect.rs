//! Save-type detection by library ID string (GBATEK "GBA Cart Backup IDs").

/// Which backup chip a cartridge carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveKind {
    None,
    /// 32 KiB SRAM (or FRAM, which software sees identically).
    Sram,
    Flash64,
    Flash128,
    /// 512 B or 8 KiB; fixed by the first DMA.
    Eeprom,
}

/// `--save-type`: which backup chip to give the cartridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SaveChoice {
    /// Detect from the ROM's library ID string ([`detect_save`]).
    #[default]
    Auto,
    /// No backup chip, whatever the ROM says.
    None,
    Sram,
    Flash64,
    Flash128,
    /// 512 bytes (6-bit addresses).
    Eeprom512,
    /// 8 KiB (14-bit addresses).
    Eeprom8k,
}

impl SaveChoice {
    /// The byte a save state or a recording stores.
    pub const fn id(self) -> u8 {
        match self {
            Self::Auto => 0,
            Self::None => 1,
            Self::Sram => 2,
            Self::Flash64 => 3,
            Self::Flash128 => 4,
            Self::Eeprom512 => 5,
            Self::Eeprom8k => 6,
        }
    }

    /// The inverse of [`SaveChoice::id`]; `None` for a byte no choice writes.
    pub const fn from_id(id: u8) -> Option<Self> {
        Some(match id {
            0 => Self::Auto,
            1 => Self::None,
            2 => Self::Sram,
            3 => Self::Flash64,
            4 => Self::Flash128,
            5 => Self::Eeprom512,
            6 => Self::Eeprom8k,
            _ => return Option::None,
        })
    }
}

/// ID strings (the version digits after `_V` are ignored), in no particular order.
const IDS: [(&[u8], SaveKind); 6] = [
    (b"EEPROM_V", SaveKind::Eeprom),
    (b"SRAM_V", SaveKind::Sram),
    (b"SRAM_F_V", SaveKind::Sram),
    (b"FLASH_V", SaveKind::Flash64),
    (b"FLASH512_V", SaveKind::Flash64),
    (b"FLASH1M_V", SaveKind::Flash128),
];

/// Scan `rom` at word-aligned offsets; the first (lowest-offset) ID wins. No ID
/// means no save: GBATEK notes that without `FLASH1M_V` a flash defaults to 64 KiB,
/// but a ROM with no string at all is treated as having no backup chip.
pub fn detect_save(rom: &[u8]) -> SaveKind {
    for word in rom.chunks(4).enumerate().map(|(i, _)| i * 4) {
        let tail = &rom[word..];
        // Every ID starts with one of these three bytes (EEPROM, SRAM, FLASH); skip
        // the rest cheaply.
        if !matches!(tail[0], b'E' | b'S' | b'F') {
            continue;
        }
        for (id, kind) in IDS {
            if tail.starts_with(id) {
                return kind;
            }
        }
    }
    SaveKind::None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rom_with(at: usize, s: &[u8]) -> Vec<u8> {
        let mut rom = vec![0; 0x400];
        rom[at..at + s.len()].copy_from_slice(s);
        rom
    }

    #[test]
    fn finds_every_id() {
        let cases: [(&[u8], SaveKind); 6] = [
            (b"EEPROM_V124", SaveKind::Eeprom),
            (b"SRAM_V113", SaveKind::Sram),
            (b"FLASH_V126", SaveKind::Flash64),
            (b"FLASH512_V131", SaveKind::Flash64),
            (b"FLASH1M_V103", SaveKind::Flash128),
            (b"SRAM_F_V100", SaveKind::Sram),
        ];
        for (s, kind) in cases {
            assert_eq!(detect_save(&rom_with(0xC0, s)), kind, "{s:?}");
        }
    }

    #[test]
    fn ids_must_be_word_aligned() {
        assert_eq!(detect_save(&rom_with(0xC1, b"SRAM_V113")), SaveKind::None);
        assert_eq!(detect_save(&rom_with(0xC2, b"FLASH_V126")), SaveKind::None);
    }

    #[test]
    fn jsmolka_style_split_id_with_blanks() {
        // `dw 'SRAM'`, `dw '_V  '`.
        assert_eq!(detect_save(&rom_with(0x100, b"SRAM_V  ")), SaveKind::Sram);
    }

    #[test]
    fn no_id_means_no_save_and_first_hit_wins() {
        assert_eq!(detect_save(&vec![0xAA; 0x400]), SaveKind::None);
        let mut rom = rom_with(0x100, b"FLASH1M_V103");
        rom[0x200..0x208].copy_from_slice(b"SRAM_V11");
        assert_eq!(detect_save(&rom), SaveKind::Flash128);
    }

    #[test]
    fn id_at_the_very_end() {
        let mut rom = vec![0; 0x100];
        rom.extend_from_slice(b"SRAM_V11");
        assert_eq!(detect_save(&rom), SaveKind::Sram);
    }
}
