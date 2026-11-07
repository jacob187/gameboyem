//! The memory map: region reads and writes by address and width. Owned by
//! P1-SYS. GBATEK "GBA Memory Map".
//!
//! Decode is on `addr >> 24`; RAM regions are direct little-endian slice accesses
//! on the width-aligned offset, never per-byte composition. Wait-state costs are
//! the caller's business ([`Bus::cost`]); nothing here touches the clock.
//!
//! Bus-width quirks implemented here:
//! * Palette and BG-VRAM byte writes store the byte in both halves of the
//!   halfword; OBJ-VRAM and OAM byte writes are ignored.
//! * SRAM/Flash is an 8-bit bus: a halfword/word read returns the byte repeated,
//!   and a halfword/word write stores one byte, the lane selected by the low
//!   address bits, at the *unaligned* address (so the save window is decoded
//!   before the alignment the other regions get).
//! * Unmapped regions, and the BIOS while the CPU runs outside it, read as the
//!   open-bus value (DECISIONS #52).

use crate::bus::{Bus, Width};

/// VRAM is 96 KiB mirrored in 128 KiB steps; the top 32 KiB of each 128 KiB
/// window mirrors the 32 KiB OBJ area above it (0x1_8000..0x2_0000 -> 0x1_0000..).
pub(crate) fn vram_index(addr: u32) -> usize {
    let a = addr & 0x1_FFFF;
    (if a >= 0x1_8000 { a - 0x8000 } else { a }) as usize
}

#[inline(always)]
pub(crate) fn rd(s: &[u8], off: usize, w: Width) -> u32 {
    let off = off & !(w as usize - 1);
    match w {
        Width::B => u32::from(s[off]),
        Width::H => u32::from(u16::from_le_bytes([s[off], s[off + 1]])),
        Width::W => u32::from_le_bytes([s[off], s[off + 1], s[off + 2], s[off + 3]]),
    }
}

#[inline(always)]
fn wr(s: &mut [u8], off: usize, v: u32, w: Width) {
    let off = off & !(w as usize - 1);
    match w {
        Width::B => s[off] = v as u8,
        Width::H => s[off..off + 2].copy_from_slice(&(v as u16).to_le_bytes()),
        Width::W => s[off..off + 4].copy_from_slice(&v.to_le_bytes()),
    }
}

impl Bus {
    /// Read `w` at `addr` as a data access (CPU load or DMA), with side effects:
    /// the EEPROM's serial read pointer. Only halfword reads advance it, since
    /// the chip is accessed in 16-bit units through DMA3 (GBATEK "GBA Cart Backup
    /// EEPROM"); byte and word reads of that region see plain ROM contents and
    /// leave the chip alone (code-review S#7). Opcode fetches never come here.
    /// No time is charged.
    #[inline]
    pub(crate) fn read_value(&mut self, addr: u32, w: Width) -> u32 {
        if w == Width::H && self.cart.eeprom_at(addr) {
            return u32::from(self.cart.eeprom_read());
        }
        // P2-APU (code-review A2#1): a sound register or wave RAM read sees the
        // APU at the current cycle, not at the last sync. `peek*` stays pure.
        if addr >> 24 == 4 && (0x60..0xA0).contains(&(addr & 0x00FF_FFFF)) {
            self.apu.advance_to(self.sched.now());
        }
        self.read_region(addr, w, false)
    }

    /// The side-effect-free read. With `peek` the BIOS isn't read-protected and
    /// the EEPROM region reads as ROM.
    pub(crate) fn read_region(&self, addr: u32, w: Width, peek: bool) -> u32 {
        match addr >> 24 {
            0x00 => {
                if addr < crate::BIOS_SIZE as u32 {
                    let a = addr & !(w as u32 - 1);
                    if peek || self.fetching_bios {
                        match w {
                            Width::B => u32::from(self.bios.read8(a)),
                            Width::H => {
                                u32::from(self.bios.read8(a))
                                    | u32::from(self.bios.read8(a + 1)) << 8
                            }
                            Width::W => (0..4)
                                .fold(0, |v, i| v | u32::from(self.bios.read8(a + i)) << (8 * i)),
                        }
                    } else {
                        // Read protection: the last opcode fetched from the BIOS.
                        match w {
                            Width::B => self.bios_latch >> ((addr & 3) * 8) & 0xFF,
                            Width::H => self.bios_latch >> ((addr & 2) * 8) & 0xFFFF,
                            Width::W => self.bios_latch,
                        }
                    }
                } else {
                    self.open_bus_value(addr, w)
                }
            }
            0x02 => rd(&self.ewram, addr as usize & 0x3_FFFF, w),
            0x03 => rd(&self.iwram, addr as usize & 0x7FFF, w),
            0x04 => self.read_io_value(addr, w),
            0x05 => rd(&*self.ppu.palette, addr as usize & 0x3FF, w),
            0x06 => rd(&*self.ppu.vram, vram_index(addr), w),
            0x07 => rd(&*self.ppu.oam, addr as usize & 0x3FF, w),
            0x08..=0x0D => self.read_rom(addr, w),
            0x0E | 0x0F => {
                let b = u32::from(self.cart.read_save8(addr & 0xFFFF));
                b * match w {
                    Width::B => 1,
                    Width::H => 0x0101,
                    Width::W => 0x0101_0101,
                }
            }
            _ => self.open_bus_value(addr, w),
        }
    }

    /// A cartridge-ROM read of `w` (the GPIO port shadows ROM at 0xC4..0xC9 once
    /// it is enabled). The address range is only tested when the cartridge has
    /// enabled GPIO reads, so the common case is one flag test.
    #[inline]
    pub(crate) fn read_rom(&self, addr: u32, w: Width) -> u32 {
        let off = addr & 0x01FF_FFFF;
        if self.cart.gpio_reads() {
            // P3-CART: the GPIO port overlays ROM at 0xC4..0xC9 once enabled.
            if let Some(v) = self.cart.gpio_read(off, w) {
                return v;
            }
        }
        match w {
            Width::B => u32::from(self.cart.read_rom8(off)),
            Width::H => u32::from(self.cart.read_rom16(off & !1)),
            Width::W => self.cart.read_rom32(off & !3),
        }
    }

    /// Write `w` at `addr`. No time is charged.
    pub(crate) fn write_value(&mut self, addr: u32, v: u32, w: Width) {
        match addr >> 24 {
            0x02 => wr(&mut self.ewram, addr as usize & 0x3_FFFF, v, w),
            0x03 => wr(&mut self.iwram, addr as usize & 0x7FFF, v, w),
            0x04 => self.write_io_value(addr, v, w),
            0x05 => {
                let off = addr as usize & 0x3FF;
                if w == Width::B {
                    wr(
                        &mut *self.ppu.palette,
                        off,
                        u32::from(v as u8) * 0x0101,
                        Width::H,
                    );
                } else {
                    wr(&mut *self.ppu.palette, off, v, w);
                }
            }
            0x06 => {
                let i = vram_index(addr);
                if w == Width::B {
                    // BG VRAM duplicates the byte; OBJ VRAM ignores byte writes.
                    // The boundary depends on the mode (GBATEK "LCD VRAM").
                    if i < self.bg_vram_end() {
                        wr(
                            &mut *self.ppu.vram,
                            i,
                            u32::from(v as u8) * 0x0101,
                            Width::H,
                        );
                    }
                } else {
                    wr(&mut *self.ppu.vram, i, v, w);
                }
            }
            0x07 => {
                if w != Width::B {
                    wr(&mut *self.ppu.oam, addr as usize & 0x3FF, v, w);
                }
            }
            0x0D if w == Width::H && self.cart.eeprom_at(addr) => {
                self.cart.eeprom_write(v as u16);
            }
            // P3-CART: stores to the GPIO registers (0xC4..0xC9 of ROM space).
            0x08..=0x0D if (0xC4..0xCA).contains(&(addr & 0x01FF_FFFF)) => {
                let now = self.sched.now();
                if self.cart.gpio_write(addr & 0x01FF_FFFF, v, w, now) {
                    self.irq.raise(crate::irq::Interrupt::GamePak);
                }
            }
            0x0E | 0x0F => {
                // One byte, from the lane the address selects, at the exact address.
                let lane = addr & (w as u32 - 1);
                self.cart
                    .write_save8(addr & 0xFFFF, (v >> (8 * lane)) as u8);
            }
            _ => {}
        }
    }

    /// End of the BG part of VRAM: 0x1_0000 in the tile modes (0-2), 0x1_4000 in
    /// the bitmap modes (3-5).
    #[inline]
    fn bg_vram_end(&self) -> usize {
        if self.ppu.dispcnt() & 7 >= 3 {
            0x1_4000
        } else {
            0x1_0000
        }
    }

    /// Read a byte with no side effects and no time cost (test and debug hook).
    pub fn peek8(&self, addr: u32) -> u8 {
        self.read_region(addr, Width::B, true) as u8
    }

    /// Read the halfword at `addr & !1` with no side effects.
    pub fn peek16(&self, addr: u32) -> u16 {
        self.read_region(addr, Width::H, true) as u16
    }

    /// Read the word at `addr & !3` with no side effects.
    pub fn peek32(&self, addr: u32) -> u32 {
        self.read_region(addr, Width::W, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vram_mirrors_obj_area() {
        assert_eq!(vram_index(0x0601_7FFF), 0x1_7FFF);
        assert_eq!(vram_index(0x0601_8000), 0x1_0000);
        assert_eq!(vram_index(0x0602_0000), 0);
    }

    #[test]
    fn slice_helpers_align_and_are_little_endian() {
        let mut m = [0u8; 8];
        wr(&mut m, 5, 0xAABB_CCDD, Width::W);
        assert_eq!(m, [0, 0, 0, 0, 0xDD, 0xCC, 0xBB, 0xAA]);
        assert_eq!(rd(&m, 7, Width::H), 0xAABB);
        assert_eq!(rd(&m, 5, Width::B), 0xCC);
    }
}
