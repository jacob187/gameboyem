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
    /// While OAM DMA is running the CPU can only reach HRAM; everything else
    /// reads 0xFF. (Simplification of the real bus-conflict behaviour, Pan Docs
    /// "OAM DMA Transfer"; refined in Phase 3.)
    pub(crate) fn cpu_read(&self, addr: u16) -> u8 {
        if self.dma.is_blocking() && !(0xFF80..=0xFFFE).contains(&addr) && addr != 0xFF46 {
            return 0xFF;
        }
        self.read_mapped(addr)
    }

    pub(crate) fn cpu_write(&mut self, addr: u16, value: u8) {
        if self.dma.is_blocking() && !(0xFF80..=0xFFFE).contains(&addr) && addr != 0xFF46 {
            return;
        }
        self.write_mapped(addr, value);
    }

    /// Source read for an OAM DMA byte. DMA sources above 0xDFFF map onto echo RAM.
    pub(crate) fn read_dma_source(&self, addr: u16) -> u8 {
        let addr = if addr >= 0xE000 { addr - 0x2000 } else { addr };
        self.read_mapped(addr)
    }

    /// Side-effect-free read of the full memory map (also used by debug tooling).
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
            0xFE00..=0xFE9F => {
                if self.dma.is_blocking() {
                    0xFF
                } else {
                    self.ppu.read_oam(addr)
                }
            }
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
            0xFE00..=0xFE9F => {
                if !self.dma.is_blocking() {
                    self.ppu.write_oam(addr, value);
                }
            }
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
            0xFF00 => self.joypad.write(value),
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
