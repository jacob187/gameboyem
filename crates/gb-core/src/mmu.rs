//! DMG memory map decoding (Pan Docs: "Memory Map", "I/O Ranges").
//!
//! ```text
//! 0000-7FFF  cartridge ROM (MBC registers on write)
//! 8000-9FFF  VRAM
//! A000-BFFF  cartridge RAM
//! C000-DFFF  WRAM
//! E000-FDFF  echo of C000-DDFF
//! FE00-FE9F  OAM
//! FEA0-FEFF  unusable
//! FF00-FF7F  I/O registers
//! FF80-FFFE  HRAM
//! FFFF       IE
//! ```

use crate::bus::Bus;

impl Bus {
    /// A CPU-initiated read (no cycle cost here; [`crate::bus::CpuBus::read`] ticks).
    ///
    /// OAM DMA bus conflicts (DMG, Pan Docs "OAM DMA Transfer"), evaluated for
    /// the cycle whose component tick just ran:
    /// * OAM reads `0xFF`.
    /// * An access to the bus the DMA source is on (external bus: ROM, cartridge
    ///   RAM, WRAM/echo; video bus: VRAM) reads the byte the DMA engine is moving.
    /// * HRAM, I/O registers (incl. `0xFF46`) and IE are always reachable.
    pub(crate) fn cpu_read(&self, addr: u16) -> u8 {
        if self.dma.is_blocking() && (0xFE00..=0xFE9F).contains(&addr) {
            return 0xFF;
        }
        if self.dma.conflicts_with(addr) {
            return self.dma.data_byte();
        }
        self.read_mapped(addr)
    }

    /// A CPU-initiated write. Writes that hit a DMA bus conflict or OAM during
    /// a transfer are dropped.
    pub(crate) fn cpu_write(&mut self, addr: u16, value: u8) {
        if self.dma.is_blocking() && (0xFE00..=0xFE9F).contains(&addr) {
            return;
        }
        if self.dma.conflicts_with(addr) {
            return;
        }
        self.write_mapped(addr, value);
    }

    /// Source read for an OAM DMA byte. DMA sources above 0xDFFF map onto echo RAM.
    pub(crate) fn read_dma_source(&self, addr: u16) -> u8 {
        let addr = if addr >= 0xE000 { addr - 0x2000 } else { addr };
        self.read_mapped(addr)
    }

    /// Read the memory map as the CPU would see it now, without side effects
    /// (`&self`: no clock advances). Not a raw dump: VRAM reads 0xFF while the
    /// PPU is in mode 3, OAM in modes 2 and 3, and cartridge RAM goes through
    /// the mapper (see `GameBoy::peek`). OAM DMA bus conflicts are not applied.
    pub fn peek(&self, addr: u16) -> u8 {
        self.read_mapped(addr)
    }

    fn read_mapped(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7FFF => self.cart.read_rom(addr),
            0x8000..=0x9FFF => self.ppu.read_vram(addr),
            0xA000..=0xBFFF => self.cart.read_ram(addr),
            0xC000..=0xDFFF => self.wram[(addr - 0xC000) as usize],
            0xE000..=0xFDFF => self.wram[(addr - 0xE000) as usize],
            0xFE00..=0xFE9F => self.ppu.read_oam(addr),
            0xFEA0..=0xFEFF => 0xFF,
            0xFF00..=0xFF7F => self.read_io(addr),
            0xFF80..=0xFFFE => self.hram[(addr - 0xFF80) as usize],
            0xFFFF => self.interrupts.read_ie(),
        }
    }

    fn write_mapped(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000..=0x7FFF => self.cart.write_rom(addr, value),
            0x8000..=0x9FFF => self.ppu.write_vram(addr, value),
            0xA000..=0xBFFF => self.cart.write_ram(addr, value),
            0xC000..=0xDFFF => self.wram[(addr - 0xC000) as usize] = value,
            0xE000..=0xFDFF => self.wram[(addr - 0xE000) as usize] = value,
            0xFE00..=0xFE9F => self.ppu.write_oam(addr, value),
            0xFEA0..=0xFEFF => {}
            0xFF00..=0xFF7F => self.write_io(addr, value),
            0xFF80..=0xFFFE => self.hram[(addr - 0xFF80) as usize] = value,
            0xFFFF => self.interrupts.write_ie(value),
        }
    }

    fn read_io(&self, addr: u16) -> u8 {
        match addr {
            0xFF00 => self.joypad.read(),
            0xFF01 | 0xFF02 => self.serial.read(addr),
            0xFF04..=0xFF07 => self.timer.read(addr),
            0xFF0F => self.interrupts.read_if(),
            0xFF10..=0xFF3F => self.apu.read(addr),
            0xFF46 => self.dma.read_reg(),
            0xFF40..=0xFF4B => self.ppu.read_reg(addr),
            // FF50 (boot ROM disable) and every unmapped DMG register read 0xFF.
            _ => 0xFF,
        }
    }

    fn write_io(&mut self, addr: u16, value: u8) {
        match addr {
            0xFF00 => self.joypad.write(value, &mut self.interrupts),
            0xFF01 | 0xFF02 => self.serial.write(addr, value),
            0xFF04..=0xFF07 => self.timer.write(addr, value, &mut self.interrupts),
            0xFF0F => self.interrupts.write_if(value),
            0xFF10..=0xFF3F => self.apu.write(addr, value),
            0xFF46 => self.dma.start(value),
            0xFF40..=0xFF4B => self.ppu.write_reg(addr, value, &mut self.interrupts),
            _ => {}
        }
    }
}
