//! Cartridges: header parsing, the [`Cartridge`] trait and MBC implementations.
//!
//! Pan Docs: "The Cartridge Header", "MBCs".

mod mbc1;
mod mbc3;
mod mbc5;
mod rom_only;

use std::fmt;

use crate::state::{StateError, StateReader, StateWriter};

pub use mbc1::Mbc1;
pub use mbc3::Mbc3;
pub use mbc5::Mbc5;
pub use rom_only::RomOnly;

/// Size of one switchable ROM bank.
pub const ROM_BANK_SIZE: usize = 0x4000;
/// Size of one switchable external RAM bank.
pub const RAM_BANK_SIZE: usize = 0x2000;

/// Offset of a CPU address (`0xA000..=0xBFFF`) within the 8 KiB RAM window.
/// Masks rather than subtracts, so it can never underflow.
#[inline]
pub(crate) fn ram_window_offset(addr: u16) -> usize {
    addr as usize & (RAM_BANK_SIZE - 1)
}

/// The memory-bank controller interface every cartridge implements.
///
/// # Contract (frozen; propose changes to the orchestrator)
///
/// * `read_rom`/`write_rom` receive CPU addresses `0x0000..=0x7FFF`.
///   Writes to this range are MBC register writes.
/// * `read_ram`/`write_ram` receive CPU addresses `0xA000..=0xBFFF`.
///   Disabled or absent RAM reads `0xFF` and ignores writes.
/// * `tick_mcycle` is called once per M-cycle by [`crate::bus::Bus`], but only
///   for a header with `mbc == Mbc3 && has_rtc` (the bus skips the virtual call
///   otherwise); default no-op. It must not read the host clock: RTC time
///   advances with emulated time only.
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

    /// Monotonic counter, meaningful only when [`Cartridge::battery_ram`] is
    /// `Some`. It advances when the program changes the persistent state: a
    /// cart-RAM write that changes a byte, an RTC register write, an RTC latch
    /// that changes the latched registers, or a successful
    /// [`Cartridge::load_battery_ram`].
    ///
    /// It deliberately does **not** advance while the MBC3 RTC free-runs, so a
    /// host can poll it to skip pointless periodic saves (the live clock makes
    /// every `battery_ram()` image differ) and still write one final
    /// `battery_ram()` image on quit, which captures the live clock.
    ///
    /// The default (constant 0) means "unknown"; every built-in mapper
    /// overrides it.
    fn battery_generation(&self) -> u64 {
        0
    }

    /// Restore persistent RAM from a `.sav` file.
    fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError>;

    /// Write the mapper's full state (registers, RAM, RTC) for a save state.
    /// The battery generation counter is host bookkeeping and is not included.
    fn save_state(&self, w: &mut StateWriter);

    /// Restore from [`Cartridge::save_state`] output. A successful load
    /// advances [`Cartridge::battery_generation`], so the host persists the
    /// restored RAM. May leave the cartridge partly overwritten on error.
    fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError>;
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
    /// Title: 0x0134 up to the first NUL, at most 15 bytes (0x0134..=0x0142), or
    /// 11 bytes (0x0134..=0x013E) when the CGB flag is 0x80/0xC0, since the
    /// 4 bytes before the flag are then the manufacturer code. Byte 0x0143 is
    /// included only on old DMG carts where it is a printable title character.
    /// Trailing spaces are stripped and non-ASCII bytes replaced by `?`.
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
    /// Raw CGB flag byte (0x0143): 0x80 = CGB-enhanced, 0xC0 = CGB-only. On old
    /// carts this byte may be a title character. Kept for future CGB support;
    /// DMG ignores it.
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
        let cgb_flag = rom[0x143];
        let title_end = if matches!(cgb_flag, 0x80 | 0xC0) {
            0x13F // 0x13F..=0x142 is the manufacturer code, 0x143 the flag
        } else if cgb_flag.is_ascii_graphic() || cgb_flag == b' ' {
            0x144 // pre-CGB cart with a full 16-character title
        } else {
            0x143
        };
        let title = rom[0x134..title_end]
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
            cgb_flag,
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
        MbcKind::Mbc5 => Box::new(Mbc5::new(rom, header)),
        MbcKind::Mbc3 => Box::new(Mbc3::new(rom, header)),
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

    fn snapshot(cart: &dyn Cartridge) -> Vec<u8> {
        let mut w = StateWriter::new();
        cart.save_state(&mut w);
        w.into_bytes()
    }

    /// Every mapper: banking, RAM and (MBC3) the RTC survive a save-state
    /// round trip, and loading advances the battery generation.
    #[test]
    fn mapper_state_round_trips() {
        use test_util::banked_rom;
        // ROM+RAM+BATTERY, MBC1+RAM+BATTERY, MBC3+TIMER+RAM+BATTERY, MBC5+RAM+BATTERY.
        for (cart_type, ram_code) in [(0x09, 0x02), (0x03, 0x03), (0x10, 0x03), (0x1B, 0x03)] {
            let banks = if cart_type == 0x09 { 2 } else { 8 };
            let rom = banked_rom(banks, cart_type, if banks == 2 { 0 } else { 2 }, ram_code);
            let mut a = from_rom(rom.clone()).unwrap();
            a.write_rom(0x0000, 0x0A);
            a.write_rom(0x2000, 0x05);
            a.write_rom(0x6000, 0x01);
            a.write_rom(0x4000, 0x02);
            a.write_ram(0xA123, 0x5A);
            if a.header().has_rtc {
                a.write_rom(0x4000, 0x08);
                a.write_ram(0xA000, 30);
                for _ in 0..(1 << 20) + 5 {
                    a.tick_mcycle();
                }
                a.write_rom(0x6000, 0x00);
                a.write_rom(0x6000, 0x01);
                a.write_rom(0x6000, 0x00);
            }
            let snap = snapshot(a.as_ref());

            let mut b = from_rom(rom).unwrap();
            let generation = b.battery_generation();
            let mut r = StateReader::new(&snap);
            b.load_state(&mut r).unwrap();
            r.finish().unwrap();
            assert!(b.battery_generation() > generation, "type {cart_type:#x}");
            assert_eq!(snapshot(b.as_ref()), snap, "type {cart_type:#x}");
            assert_eq!(a.battery_ram(), b.battery_ram(), "type {cart_type:#x}");
            for addr in (0x0000..0x8000).step_by(0x0800) {
                assert_eq!(
                    a.read_rom(addr),
                    b.read_rom(addr),
                    "type {cart_type:#x} {addr:#x}"
                );
            }
            for addr in (0xA000..0xC000).step_by(0x0101) {
                assert_eq!(
                    a.read_ram(addr),
                    b.read_ram(addr),
                    "type {cart_type:#x} {addr:#x}"
                );
            }
            if a.header().has_rtc {
                // The free-running clock and the latch keep going in step.
                for _ in 0..(1 << 20) {
                    a.tick_mcycle();
                    b.tick_mcycle();
                }
                assert_eq!(snapshot(a.as_ref()), snapshot(b.as_ref()));
            }
        }
    }

    #[test]
    fn mapper_state_rejects_other_ram_size() {
        use test_util::banked_rom;
        let small = from_rom(banked_rom(8, 0x1B, 2, 0x02)).unwrap();
        let mut big = from_rom(banked_rom(8, 0x1B, 2, 0x03)).unwrap();
        let snap = snapshot(small.as_ref());
        assert_eq!(
            big.load_state(&mut StateReader::new(&snap)),
            Err(StateError::Invalid("cart RAM size"))
        );
    }

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
    fn title_excludes_cgb_flag_and_manufacturer_code() {
        // Unterminated "POKEMON_GLD" + manufacturer code "AAUE" + CGB flag 0x80.
        let mut rom = test_util::banked_rom(2, 0x00, 0, 0);
        rom[0x134..0x144].copy_from_slice(b"POKEMON_GLDAAUE\x80");
        let h = CartridgeHeader::parse(&rom).unwrap();
        assert_eq!(h.title, "POKEMON_GLD");
        assert_eq!(h.cgb_flag, 0x80);
        rom[0x143] = 0xC0;
        assert_eq!(CartridgeHeader::parse(&rom).unwrap().title, "POKEMON_GLD");
        // Non-CGB flag byte that is not a title character: 15-byte title.
        rom[0x143] = 0x00;
        let h = CartridgeHeader::parse(&rom).unwrap();
        assert_eq!(h.title, "POKEMON_GLDAAUE");
        assert_eq!(h.cgb_flag, 0x00);
        // Old DMG cart with a full 16-character title keeps its last character.
        rom[0x134..0x144].copy_from_slice(b"ABCDEFGHIJKLMNOP");
        let h = CartridgeHeader::parse(&rom).unwrap();
        assert_eq!(h.title, "ABCDEFGHIJKLMNOP");
        // NUL still terminates early.
        rom[0x134..0x144].copy_from_slice(b"HI\0JUNKJUNKJUNK\x80");
        assert_eq!(CartridgeHeader::parse(&rom).unwrap().title, "HI");
    }

    #[test]
    fn ram_window_offset_never_underflows() {
        assert_eq!(ram_window_offset(0xA000), 0);
        assert_eq!(ram_window_offset(0xBFFF), 0x1FFF);
        assert_eq!(ram_window_offset(0x0000), 0, "out-of-window addr is safe");
    }

    /// All mappers must decode A000-BFFF identically, including for RAM sizes
    /// that are not a multiple of the window (2 KiB) and for stray addresses.
    #[test]
    fn all_mappers_decode_ram_addresses_alike() {
        for cart_type in [0x08u8, 0x02, 0x12, 0x1A] {
            let mut c = from_rom(test_util::banked_rom(2, cart_type, 0, 0x01)).unwrap();
            c.write_rom(0x0000, 0x0A);
            c.write_ram(0xA005, 0x77);
            assert_eq!(c.read_ram(0xA005), 0x77, "type {cart_type:#04x}");
            assert_eq!(
                c.read_ram(0xA805),
                0x77,
                "2 KiB mirror, type {cart_type:#04x}"
            );
            // Addresses below the window (never sent by the bus) must not panic.
            let _ = c.read_ram(0x0000);
            c.write_ram(0x0000, 1);
        }
    }

    /// Writing a changed RAM byte advances the generation; rewriting the same
    /// value, writing while disabled, and reads do not; loading a save does.
    #[test]
    fn battery_generation_tracks_ram_changes() {
        for cart_type in [0x09u8, 0x03, 0x13, 0x1B] {
            let mut c = from_rom(test_util::banked_rom(2, cart_type, 0, 0x02)).unwrap();
            let t = format!("type {cart_type:#04x}");
            let g0 = c.battery_generation();
            if cart_type != 0x09 {
                // ROM-only RAM has no enable register.
                c.write_ram(0xA000, 1);
                assert_eq!(c.battery_generation(), g0, "disabled write, {t}");
            }
            c.write_rom(0x0000, 0x0A);
            c.write_ram(0xA000, 1);
            let g1 = c.battery_generation();
            assert!(g1 > g0, "{t}");
            c.write_ram(0xA000, 1);
            c.read_ram(0xA000);
            c.tick_mcycle();
            assert_eq!(c.battery_generation(), g1, "{t}");
            let save = c.battery_ram().unwrap();
            c.load_battery_ram(&save).unwrap();
            assert!(c.battery_generation() > g1, "{t}");
        }
    }

    #[test]
    fn truncated_rom_is_padded() {
        let rom = normalize_rom(vec![0; 0x150], 0x8000);
        assert_eq!(rom.len(), 0x8000);
        let rom = normalize_rom(vec![0; 3 * ROM_BANK_SIZE], 0x8000);
        assert_eq!(rom.len(), 4 * ROM_BANK_SIZE);
    }

    /// Every combination of the three variable header bytes, on ROMs shorter
    /// and longer than declared, must either be rejected or be fully usable.
    #[test]
    fn garbage_headers_never_panic() {
        for cart_type in 0..=255u8 {
            for rom_code in [0x00, 0x01, 0x05, 0x08, 0x09, 0x52, 0xFF] {
                for ram_code in [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0xFF] {
                    for banks in [2usize, 3] {
                        let rom = test_util::banked_rom(banks, cart_type, rom_code, ram_code);
                        let Ok(mut c) = from_rom(rom) else { continue };
                        for v in [0x00, 0x01, 0x0A, 0x7F, 0xFF] {
                            for a in [0x0000u16, 0x2000, 0x3000, 0x4000, 0x6000, 0x7FFF] {
                                c.write_rom(a, v);
                                for r in [0x0000u16, 0x3FFF, 0x4000, 0x7FFF] {
                                    c.read_rom(r);
                                }
                            }
                            for r in [0xA000u16, 0xA7FF, 0xBFFF] {
                                c.write_ram(r, v);
                                c.read_ram(r);
                            }
                            c.tick_mcycle();
                        }
                        let _ = c.battery_ram();
                        let _ = c.load_battery_ram(&[0xFF; 7]);
                    }
                }
            }
        }
    }

    /// Random ROM content with a *valid* header (supported type, ROM size
    /// <= 512 KiB, any RAM size), so every mapper is actually constructed and
    /// exercised with random register writes, reads, ticks and saves.
    #[test]
    fn random_bytes_never_panic() {
        const TYPES: [u8; 17] = [
            0x00, 0x08, 0x09, 0x01, 0x02, 0x03, 0x0F, 0x10, 0x11, 0x12, 0x13, 0x19, 0x1A, 0x1B,
            0x1C, 0x1D, 0x1E,
        ];
        let mut x = 0x2545_F491u32;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let (mut built, mut per_kind) = (0usize, [0usize; 4]);
        for len in [0x150usize, 0x4000, 0x8000, 0x2_0000] {
            for _ in 0..200 {
                let mut rom: Vec<u8> = (0..len).map(|_| next() as u8).collect();
                rom[0x147] = TYPES[next() as usize % TYPES.len()];
                rom[0x148] = (next() % 6) as u8;
                rom[0x149] = (next() % 6) as u8;
                let mut c = from_rom(rom).expect("valid header must load");
                built += 1;
                per_kind[c.header().mbc as usize] += 1;
                for _ in 0..64 {
                    let r = next();
                    match r % 5 {
                        0 => c.write_rom((r >> 8) as u16 & 0x7FFF, (r >> 24) as u8),
                        1 => c.write_ram(0xA000 | (r >> 8) as u16 & 0x1FFF, (r >> 24) as u8),
                        2 => {
                            c.read_rom((r >> 8) as u16 & 0x7FFF);
                        }
                        3 => {
                            c.read_ram(0xA000 | (r >> 8) as u16 & 0x1FFF);
                        }
                        _ => c.tick_mcycle(),
                    }
                }
                if let Some(save) = c.battery_ram() {
                    c.load_battery_ram(&save).expect("own save must reload");
                }
            }
        }
        assert_eq!(built, 800, "every valid-header ROM must construct");
        assert!(
            per_kind.iter().all(|&n| n > 100),
            "mapper mix: {per_kind:?}"
        );
    }
}
