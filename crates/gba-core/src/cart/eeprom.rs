//! EEPROM backup (512 B or 8 KiB), bit-serial over DMA3. GBATEK "GBA Cart Backup
//! EEPROM".
//!
//! Every halfword written to the cartridge's EEPROM address supplies one bit (bit 0);
//! every halfword read returns one bit. A request is `11` (read) or `10` (write),
//! the 6- or 14-bit address MSB first, 64 data bits for a write, and a stop bit.
//! A read request is followed by a 68-bit read: 4 ignored bits, then 64 data
//! bits MSB first. When idle the chip reads 1 ("ready"); writes finish instantly.
//!
//! The chip size is not detectable from the ROM. It is inferred from the length of
//! the first DMA to the chip (9 or 73 bits = 6-bit addressing = 512 B; 17 or 81
//! bits = 14-bit addressing = 8 KiB) or from the size of a loaded save file.

pub const SIZE_512: usize = 512;
pub const SIZE_8K: usize = 8 * 1024;

pub struct Eeprom {
    /// Always 8 KiB so either size fits; only `size` bytes are used.
    data: Box<[u8]>,
    /// 512 or 8192 once known.
    size: Option<usize>,
    /// The size came from a DMA length, not a loaded file.
    size_confirmed: bool,
    /// Request bits received so far (MSB first) and how many.
    rx: u128,
    rx_len: u32,
    /// Data being read out and how many bits of the 68 remain.
    tx: u64,
    tx_left: u32,
}

impl Default for Eeprom {
    fn default() -> Self {
        Self {
            data: vec![0xFF; SIZE_8K].into_boxed_slice(),
            size: None,
            size_confirmed: false,
            rx: 0,
            rx_len: 0,
            tx: 0,
            tx_left: 0,
        }
    }
}

impl Eeprom {
    /// The used part of the chip, once the size is known.
    pub fn data(&self) -> Option<&[u8]> {
        self.size.map(|s| &self.data[..s])
    }

    /// Install a loaded save (length already validated as 512 or 8192).
    pub fn load(&mut self, bytes: &[u8]) {
        self.data.fill(0xFF);
        self.data[..bytes.len()].copy_from_slice(bytes);
        self.size = Some(bytes.len());
        self.size_confirmed = false;
        self.reset_serial();
    }

    fn reset_serial(&mut self) {
        self.rx = 0;
        self.rx_len = 0;
        self.tx_left = 0;
    }

    fn addr_bits(&self) -> u32 {
        // Unknown size: assume the larger chip until a DMA length says otherwise.
        if self.size == Some(SIZE_512) { 6 } else { 14 }
    }

    /// A DMA of `count` halfwords is about to write to the chip: start a fresh
    /// request, and fix the size from the first transfer's length.
    pub fn begin_write_dma(&mut self, count: u32) {
        self.reset_serial();
        if self.size_confirmed {
            return;
        }
        let size = match count {
            9 | 73 => SIZE_512,
            17 | 81 => SIZE_8K,
            _ => return,
        };
        self.size = Some(size);
        self.size_confirmed = true;
    }

    /// Shift one request bit in. Returns true if a write changed the stored data.
    pub fn write_bit(&mut self, bit: u16) -> bool {
        self.rx = (self.rx << 1) | u128::from(bit & 1);
        self.rx_len += 1;
        let n = self.addr_bits();
        if self.rx_len == 2 && self.rx & 3 < 2 {
            // Not a request prefix (`10` / `11`): drop it.
            self.reset_serial();
            return false;
        }
        let mask = 0x3FF_u128; // 8 KiB uses 10 address bits; 512 B uses 6
        let amask = if n == 6 { 0x3F } else { mask };
        if self.rx_len == 2 + n + 1 && self.rx >> (n + 1) == 0b11 {
            let addr = ((self.rx >> 1) & amask) as usize;
            self.tx = u64::from_be_bytes(self.data[addr * 8..addr * 8 + 8].try_into().unwrap());
            self.tx_left = 68;
            self.rx = 0;
            self.rx_len = 0;
        } else if self.rx_len == 2 + n + 64 + 1 {
            let prefix = self.rx >> (n + 65);
            let changed = if prefix == 0b10 {
                let addr = ((self.rx >> 65) & amask) as usize;
                let value = ((self.rx >> 1) as u64).to_be_bytes();
                let cell = &mut self.data[addr * 8..addr * 8 + 8];
                let changed = *cell != value;
                cell.copy_from_slice(&value);
                changed
            } else {
                false
            };
            self.reset_serial();
            return changed;
        }
        false
    }

    /// Shift one bit out (1 when idle).
    pub fn read_bit(&mut self) -> u16 {
        if self.tx_left == 0 {
            return 1;
        }
        self.tx_left -= 1;
        if self.tx_left >= 64 {
            0
        } else {
            ((self.tx >> self.tx_left) & 1) as u16
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(e: &mut Eeprom, bits: &[u8]) -> bool {
        e.begin_write_dma(bits.len() as u32);
        bits.iter()
            .fold(false, |c, &b| e.write_bit(u16::from(b)) | c)
    }

    fn addr_bits(addr: u32, n: u32) -> Vec<u8> {
        (0..n).rev().map(|i| ((addr >> i) & 1) as u8).collect()
    }

    fn write_req(addr: u32, n: u32, value: u64) -> Vec<u8> {
        let mut v = vec![1, 0];
        v.extend(addr_bits(addr, n));
        v.extend((0..64).rev().map(|i| ((value >> i) & 1) as u8));
        v.push(0);
        v
    }

    fn read_req(addr: u32, n: u32) -> Vec<u8> {
        let mut v = vec![1, 1];
        v.extend(addr_bits(addr, n));
        v.push(0);
        v
    }

    fn read_value(e: &mut Eeprom) -> u64 {
        for _ in 0..4 {
            assert_eq!(e.read_bit(), 0);
        }
        (0..64).fold(0, |v, _| (v << 1) | u64::from(e.read_bit()))
    }

    #[test]
    fn idle_reads_ready() {
        let mut e = Eeprom::default();
        assert_eq!(e.read_bit(), 1);
    }

    #[test]
    fn size_comes_from_the_first_dma_length() {
        for (len, size) in [(9, SIZE_512), (73, SIZE_512), (17, SIZE_8K), (81, SIZE_8K)] {
            let mut e = Eeprom::default();
            e.begin_write_dma(len);
            assert_eq!(e.data().map(<[u8]>::len), Some(size), "len {len}");
        }
        let mut e = Eeprom::default();
        e.begin_write_dma(5);
        assert!(e.data().is_none());
    }

    #[test]
    fn write_then_read_512() {
        let mut e = Eeprom::default();
        assert!(send(&mut e, &write_req(0x2A, 6, 0x0123_4567_89AB_CDEF)));
        assert_eq!(e.read_bit(), 1);
        send(&mut e, &read_req(0x2A, 6));
        assert_eq!(read_value(&mut e), 0x0123_4567_89AB_CDEF);
        assert_eq!(e.read_bit(), 1);
        assert_eq!(e.data().unwrap().len(), SIZE_512);
        assert_eq!(
            &e.data().unwrap()[0x2A * 8..0x2A * 8 + 8],
            &[0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF]
        );
    }

    #[test]
    fn write_then_read_8k() {
        let mut e = Eeprom::default();
        send(&mut e, &write_req(0x3FF, 14, 42));
        send(&mut e, &read_req(0x3FF, 14));
        assert_eq!(read_value(&mut e), 42);
        assert_eq!(e.data().unwrap().len(), SIZE_8K);
    }

    #[test]
    fn rewriting_the_same_value_is_not_a_change() {
        let mut e = Eeprom::default();
        assert!(send(&mut e, &write_req(1, 6, 7)));
        assert!(!send(&mut e, &write_req(1, 6, 7)));
    }

    #[test]
    fn loaded_size_is_overridden_by_the_first_dma() {
        let mut e = Eeprom::default();
        e.load(&vec![0; SIZE_8K]);
        e.begin_write_dma(9);
        assert_eq!(e.data().unwrap().len(), SIZE_512);
    }
}
