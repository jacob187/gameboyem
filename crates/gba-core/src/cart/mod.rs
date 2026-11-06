//! The Game Pak: ROM, header, the save backend (SRAM / Flash / EEPROM) and, from
//! P3-CART, the GPIO port with its S-3511 real-time clock.
//!
//! **Contract:** [`Cart::from_rom`], [`Cart::title`], [`CartError`], and the
//! region accessors the bus calls.
//!
//! **Save image (DECISIONS #67).** `.sav` = the backup chip's bytes, optionally
//! followed by a 24-byte RTC footer ([`RTC_FOOTER_LEN`], `rtc.rs`). A file with no
//! footer (every save made before P3-CART, or by a cartridge without a clock)
//! loads as before; a footer that is present but invalid rejects the whole file.

mod detect;
mod eeprom;
mod flash;
mod gpio;
mod rtc;

use std::fmt;

use crate::bus::Width;
use detect::detect_save;
pub use detect::{SaveChoice, SaveKind};
use eeprom::Eeprom;
use flash::Flash;
use gpio::Gpio;
pub use gpio::{GpioEffect, RtcMode};
use rtc::Rtc;
pub use rtc::{FOOTER_LEN as RTC_FOOTER_LEN, RtcTime};

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
    /// The RTC footer after the save data is present but invalid; the save is
    /// refused rather than loaded with a made-up clock.
    BadRtcFooter(&'static str),
    /// A date or time the RTC cannot hold (outside 2000-01-01..2099-12-31, or an
    /// impossible calendar day).
    BadRtcTime,
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
            Self::BadRtcFooter(why) => write!(f, "the save's RTC footer is invalid ({why})"),
            Self::BadRtcTime => write!(f, "not a time the cartridge clock can hold"),
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

/// What the host asks of the cartridge besides the ROM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CartOptions {
    /// `--save-type`.
    pub save: SaveChoice,
    /// `--rtc`.
    pub rtc: RtcMode,
}

impl Backup {
    fn new_sram() -> Self {
        Self::Sram(vec![0xFF; SRAM_SIZE].into_boxed_slice())
    }

    /// Whether a save of `len` bytes (without any RTC footer) fits this chip.
    fn accepts(&self, len: usize) -> bool {
        match self {
            Self::None => false,
            Self::Sram(_) => len == SRAM_SIZE,
            Self::Flash(f) => len == f.data().len(),
            Self::Eeprom(e) => e.accepts(len),
        }
    }

    /// What `accepts` wants, for the error message.
    fn expected(&self) -> &'static str {
        match self {
            Self::None => "no save",
            Self::Sram(_) => "32768 (SRAM)",
            Self::Flash(f) if f.data().len() == flash::SIZE_128K => "131072 (128 KiB flash)",
            Self::Flash(_) => "65536 (64 KiB flash)",
            Self::Eeprom(e) if e.accepts(eeprom::SIZE_8K) && e.accepts(eeprom::SIZE_512) => {
                "512 or 8192 (EEPROM)"
            }
            Self::Eeprom(e) if e.accepts(eeprom::SIZE_512) => "512 (EEPROM)",
            Self::Eeprom(_) => "8192 (EEPROM)",
        }
    }
}

pub struct Cart {
    rom: Vec<u8>,
    backup: Backup,
    /// The GPIO port and the clock behind it.
    gpio: Gpio,
    /// Bumps on every change to the save data and on a successful load.
    generation: u64,
}

impl Cart {
    /// A cartridge with the save type detected from the ROM and the RTC in `auto`.
    pub fn from_rom(rom: Vec<u8>) -> Result<Self, CartError> {
        Self::with_options(rom, CartOptions::default())
    }

    pub fn with_options(rom: Vec<u8>, options: CartOptions) -> Result<Self, CartError> {
        if rom.len() < HEADER_SIZE {
            return Err(CartError::TooSmall(rom.len()));
        }
        if rom.len() > crate::MAX_ROM_SIZE {
            return Err(CartError::TooLarge(rom.len()));
        }
        let backup = match options.save {
            SaveChoice::Auto => match detect_save(&rom) {
                SaveKind::None => Backup::None,
                SaveKind::Sram => Backup::new_sram(),
                SaveKind::Flash64 => Backup::Flash(Flash::new(flash::SIZE_64K)),
                SaveKind::Flash128 => Backup::Flash(Flash::new(flash::SIZE_128K)),
                SaveKind::Eeprom => Backup::Eeprom(Eeprom::default()),
            },
            SaveChoice::None => Backup::None,
            SaveChoice::Sram => Backup::new_sram(),
            SaveChoice::Flash64 => Backup::Flash(Flash::new(flash::SIZE_64K)),
            SaveChoice::Flash128 => Backup::Flash(Flash::new(flash::SIZE_128K)),
            SaveChoice::Eeprom512 => Backup::Eeprom(Eeprom::with_size(eeprom::SIZE_512)),
            SaveChoice::Eeprom8k => Backup::Eeprom(Eeprom::with_size(eeprom::SIZE_8K)),
        };
        Ok(Self {
            rom,
            backup,
            gpio: Gpio::new(options.rtc),
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
        addr >> 24 == 0x0D
            && matches!(self.backup, Backup::Eeprom(_))
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

    /// One bit read from the EEPROM (1 when idle). The first read after a
    /// CPU-driven request fixes an unknown chip size from that request's length
    /// (`Eeprom::infer_pending`).
    pub fn eeprom_read(&mut self) -> u16 {
        match &mut self.backup {
            Backup::Eeprom(e) => {
                self.generation += u64::from(e.infer_pending());
                e.read_bit()
            }
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

    /// Restore a `.sav` image at power-on (see [`Cart::load_battery_ram_at`]).
    pub fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), CartError> {
        self.load_battery_ram_at(data, 0)
    }

    /// Restore a `.sav` image at master-clock cycle `now`. The file is untrusted.
    /// Its length must be the detected chip's size (an EEPROM takes 512 or 8192
    /// unless `--save-type` fixed it and takes its size from the file), or that
    /// plus an RTC footer, which is validated field by field. Nothing changes
    /// unless the whole file is accepted.
    pub fn load_battery_ram_at(&mut self, data: &[u8], now: u64) -> Result<(), CartError> {
        if matches!(self.backup, Backup::None) {
            return Err(CartError::NoSave);
        }
        let split = data.len().saturating_sub(RTC_FOOTER_LEN);
        let (body, footer) = if self.backup.accepts(data.len()) {
            (data, None)
        } else if data.len() > RTC_FOOTER_LEN && self.backup.accepts(split) {
            (&data[..split], Some(&data[split..]))
        } else {
            return Err(CartError::SaveSize {
                got: data.len(),
                expected: self.backup.expected(),
            });
        };
        let rtc = footer.map(|f| Rtc::from_footer(f, now)).transpose()?;
        match &mut self.backup {
            Backup::None => {}
            Backup::Sram(d) => d.copy_from_slice(body),
            Backup::Flash(f) => f.load(body),
            Backup::Eeprom(e) => e.load(body),
        }
        if let Some(rtc) = rtc {
            self.gpio.load_rtc(rtc);
        }
        self.generation += 1;
        Ok(())
    }

    /// The `.sav` image at master-clock cycle `now`: [`Cart::battery_ram`] plus,
    /// once the game has used the clock or a save with a footer was loaded, the
    /// footer. `None`
    /// exactly when `battery_ram` is.
    pub fn battery_save(&self, now: u64) -> Option<Vec<u8>> {
        let mut image = self.battery_ram()?.to_vec();
        if let Some(footer) = self.gpio.footer(now) {
            image.extend_from_slice(&footer);
        }
        Some(image)
    }

    /// Start the clock at `t` at master-clock cycle `now` (the host's one entry
    /// point for real time; DECISIONS #66). It only sets the time: the save
    /// image gains no footer until the game uses the clock.
    pub fn set_rtc(&mut self, t: RtcTime, now: u64) {
        self.gpio.rtc_mut().set_time(t, now);
    }

    /// The clock's date and time at cycle `now`, whether or not the game has
    /// started using it.
    pub fn rtc_time(&self, now: u64) -> RtcTime {
        self.gpio.rtc().at(now).time()
    }

    /// The RTC's control register (test and debug hook).
    pub fn rtc_control(&self) -> u8 {
        self.gpio.rtc().control
    }

    /// Whether the GPIO port is present (see [`RtcMode`]).
    pub fn gpio_live(&self) -> bool {
        self.gpio.live()
    }

    /// A data read at ROM offset `off` (0..0x0200_0000) of width `w`, if it hits
    /// a GPIO register that is currently readable: `None` means read the ROM.
    /// Registers are 0xC4 data, 0xC6 direction, 0xC8 control; a word covering a
    /// register and ROM mixes the two.
    pub fn gpio_read(&self, off: u32, w: Width) -> Option<u32> {
        if !(0xC4..0xCA).contains(&off) {
            return None;
        }
        let half = |a: u32| self.gpio.read16(a);
        match w {
            Width::B => half(off & !1).map(|h| u32::from(h) >> ((off & 1) * 8) & 0xFF),
            Width::H => half(off & !1).map(u32::from),
            Width::W => {
                let a = off & !3;
                let (lo, hi) = (half(a), half(a + 2));
                if lo.is_none() && hi.is_none() {
                    return None;
                }
                let lo = lo.unwrap_or_else(|| self.read_rom16(a));
                let hi = hi.unwrap_or_else(|| self.read_rom16(a + 2));
                Some(u32::from(lo) | u32::from(hi) << 16)
            }
        }
    }

    /// A data write of `w` to ROM offset `off`. Only halfword and word stores
    /// reach the port (a byte store is ignored, GBATEK); a word store at 0xC4
    /// sets data and direction. Returns whether the game pak interrupt fires.
    pub fn gpio_write(&mut self, off: u32, v: u32, w: Width, now: u64) -> bool {
        let fx = match w {
            Width::B => return false,
            Width::H => self.gpio.write16(off & !1, v as u16, now),
            Width::W => self.gpio.write32(off & !3, v, now),
        };
        self.generation += u64::from(fx.rtc_changed);
        fx.irq
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
