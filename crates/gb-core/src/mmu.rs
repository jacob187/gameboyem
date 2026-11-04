//! Memory map decoding (Pan Docs: "Memory Map", "I/O Ranges").
//!
//! ```text
//! 0000-7FFF  cartridge ROM (MBC registers on write)
//! 8000-9FFF  VRAM (CGB: 2 banks, VBK)
//! A000-BFFF  cartridge RAM
//! C000-CFFF  WRAM bank 0
//! D000-DFFF  WRAM bank 1 (CGB: banks 1-7, SVBK)
//! E000-FDFF  echo of C000-DDFF
//! FE00-FE9F  OAM
//! FEA0-FEFF  unusable (reads 0x00, or 0xFF while OAM is blocked)
//! FF00-FF7F  I/O registers
//! FF80-FFFE  HRAM
//! FFFF       IE
//! ```
//!
//! CGB-only registers (KEY1, VBK, SVBK, FF72-FF77) are decoded in the fallback
//! arms of `read_io` / `write_io`, which a DMG reaches only for addresses that
//! are unmapped on it, so the DMG paths pay nothing for them. See
//! [`Hw`](crate::model::Hw) for what each variant maps.

use crate::bus::Bus;
use crate::model::Hw;

impl Bus {
    /// A CPU-initiated read (no cycle cost here; [`crate::bus::CpuBus::read`] ticks).
    ///
    /// OAM DMA bus conflicts (DMG, Pan Docs "OAM DMA Transfer"), evaluated for
    /// the cycle whose component tick just ran:
    /// * OAM (and the unusable range after it) reads `0xFF`.
    /// * An access to the bus the DMA source is on (external bus: ROM, cartridge
    ///   RAM, WRAM/echo; video bus: VRAM) reads the byte the DMA engine is moving.
    /// * HRAM, I/O registers (incl. `0xFF46`) and IE are always reachable.
    pub(crate) fn cpu_read(&self, addr: u16) -> u8 {
        if self.dma.is_blocking() && (0xFE00..=0xFEFF).contains(&addr) {
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

    /// Source read for an OAM DMA byte. DMA sources above 0xDFFF map onto echo
    /// RAM. The DMA engine is not the CPU, so VRAM is read regardless of the PPU
    /// mode (Pan Docs documents no mode-3 corruption of DMA reads; SameBoy
    /// exempts them from the VRAM lockout too).
    pub(crate) fn read_dma_source(&self, addr: u16) -> u8 {
        let addr = if addr >= 0xE000 { addr - 0x2000 } else { addr };
        match addr {
            0x8000..=0x9FFF => self.ppu.dma_read_vram(addr),
            _ => self.read_mapped(addr),
        }
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
            0xC000..=0xFDFF => self.wram[self.wram_index(addr)],
            0xFE00..=0xFE9F => self.ppu.read_oam(addr),
            // Unusable (Pan Docs "FEA0-FEFF range"): DMG reads 0xFF while OAM is
            // blocked and 0x00 otherwise. A running DMA is handled in `cpu_read`.
            0xFEA0..=0xFEFF if self.ppu.oam_blocked() => 0xFF,
            0xFEA0..=0xFEFF => 0x00,
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
            0xC000..=0xFDFF => {
                let i = self.wram_index(addr);
                self.wram[i] = value;
            }
            0xFE00..=0xFE9F => self.ppu.write_oam(addr, value),
            0xFEA0..=0xFEFF => {}
            0xFF00..=0xFF7F => self.write_io(addr, value),
            0xFF80..=0xFFFE => self.hram[(addr - 0xFF80) as usize] = value,
            0xFFFF => self.interrupts.write_ie(value),
        }
    }

    /// Index into `wram` of an address in C000-FDFF (WRAM and its echo). Bit 12
    /// picks the 4 KiB half (the echo has the same parity), and the upper half is
    /// the selected bank's window.
    #[inline]
    fn wram_index(&self, addr: u16) -> usize {
        let low = usize::from(addr & 0x0FFF);
        if addr & 0x1000 == 0 {
            low
        } else {
            self.wram_hi + low
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
            // Every register unmapped on a DMG (FF50 included) reads 0xFF there;
            // a CGB maps some of them.
            _ => self.read_cgb_io(addr),
        }
    }

    /// Reads in the range a DMG leaves unmapped. In DMG-compatibility mode a CGB
    /// maps only VBK (locked to bank 0), the palette index registers' unused
    /// bit and FF72-FF77 (mooneye `unused_hwio-C`, `boot_hwio-C`); the rest
    /// (KEY1, SVBK, HDMA, RP, ...) stays unmapped.
    fn read_cgb_io(&self, addr: u16) -> u8 {
        match (self.hw, addr) {
            (Hw::Dmg, _) => 0xFF,
            (_, 0xFF4F) => self.ppu.read_vbk(),
            (Hw::Cgb, 0xFF4D) => 0x7E | u8::from(self.key1_prepare),
            (Hw::Cgb, 0xFF70) => 0xF8 | self.svbk,
            (_, 0xFF72) => self.cgb_scratch[0],
            (_, 0xFF73) => self.cgb_scratch[1],
            (_, 0xFF75) => 0x8F | self.cgb_scratch[2],
            (_, 0xFF76 | 0xFF77) => self.apu.read_pcm(addr),
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
            _ => self.write_cgb_io(addr, value),
        }
    }

    /// Writes in the range a DMG leaves unmapped (see [`Bus::read_cgb_io`]).
    fn write_cgb_io(&mut self, addr: u16, value: u8) {
        match (self.hw, addr) {
            (Hw::Dmg, _) => {}
            // Compat mode: VBK stays on bank 0 (the bank-1 tiles and attributes
            // are CGB-mode features), so the write is dropped.
            (Hw::Cgb, 0xFF4F) => self.ppu.write_vbk(value),
            (Hw::Cgb, 0xFF4D) => self.key1_prepare = value & 1 != 0,
            (Hw::Cgb, 0xFF70) => self.set_svbk(value),
            (_, 0xFF72) => self.cgb_scratch[0] = value,
            (_, 0xFF73) => self.cgb_scratch[1] = value,
            (_, 0xFF75) => self.cgb_scratch[2] = value & 0x70,
            _ => {}
        }
    }
}
