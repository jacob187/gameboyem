//! EEPROM backup (512 B or 8 KiB), bit-serial over DMA3. GBATEK "GBA Cart Backup
//! EEPROM".
//!
//! Every halfword written to the cartridge's EEPROM address supplies one bit (bit 0);
//! every halfword read returns one bit. A request is `11` (read) or `10` (write),
//! the 6- or 14-bit address MSB first, 64 data bits for a write, and a stop bit.
//! A read request is followed by a 68-bit read: 4 ignored bits, then 64 data
//! bits MSB first. When idle the chip reads 1 ("ready"); writes finish instantly.
//!
//! The chip size is not detectable from the ROM. It is, in order of precedence:
//! forced by `--save-type eeprom512|eeprom8k`; inferred from the length of a DMA
//! to the chip (9 or 73 bits = 6-bit addressing = 512 B; 17 or 81 bits = 14-bit
//! addressing = 8 KiB), which overrides the size of a loaded save file (a 512-byte
//! game's save padded to 8 KiB by another tool shrinks to 512;
//! a real 8 KiB game never sends a 9- or 73-bit request); taken from the size of
//! a loaded save file until such a DMA; or,
//! for a game that drives the chip with CPU stores instead of DMA, inferred from
//! the length of the first request when the game first reads the chip back (the
//! request is 9/73 bits for 512 B and 17/81 for 8 KiB, and a CPU-driven game
//! has no other end-of-request marker). A request of any other length, or one
//! that completes before that read, is taken as 8 KiB. GBATEK says CPU-driven EEPROM does not work on hardware;
//! this inference is for tolerance only.

use crate::state::{StateError, check};
use emu_common::state::{StateReader, StateWriter};

pub const SIZE_512: usize = 512;
pub const SIZE_8K: usize = 8 * 1024;

pub struct Eeprom {
    /// Always 8 KiB so either size fits; only `size` bytes are used.
    data: Box<[u8]>,
    /// 512 or 8192 once known.
    size: Option<usize>,
    /// `--save-type` fixed the size, so a save file of the other size is refused.
    forced: bool,
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
            forced: false,
            rx: 0,
            rx_len: 0,
            tx: 0,
            tx_left: 0,
        }
    }
}

impl Eeprom {
    /// A blank chip configured as this one is by the cartridge options (a forced
    /// size stays; otherwise the size is still to be found).
    pub fn blank_like(&self) -> Self {
        match (self.forced, self.size) {
            (true, Some(size)) => Self::with_size(size),
            _ => Self::default(),
        }
    }

    /// A chip of known size (`--save-type`): 512 or 8192 bytes.
    pub fn with_size(size: usize) -> Self {
        Self {
            size: Some(size),
            forced: true,
            ..Self::default()
        }
    }

    /// The whole 8 KiB array (a chip of 512 bytes keeps the rest erased), the
    /// size state and the request and response shift registers, so a state taken
    /// between two bits of a DMA still completes the request.
    pub fn save_state(&self, w: &mut StateWriter) {
        w.bytes(&self.data);
        w.u32(self.size.map_or(0, |s| s as u32));
        w.bool(self.forced);
        w.u64((self.rx >> 64) as u64);
        w.u64(self.rx as u64);
        w.u32(self.rx_len);
        w.u64(self.tx);
        w.u32(self.tx_left);
    }

    /// `forced` follows the cartridge options (`--save-type eeprom512|eeprom8k`),
    /// so it must match this chip, and a forced chip keeps its size. A request
    /// being received has at most 2 + 14 + 64 = 80 bits (72 on a 512-byte chip)
    /// and a response 68.
    pub fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let mut data = vec![0; SIZE_8K].into_boxed_slice();
        r.bytes_into(&mut data)?;
        let size = match r.u32()? {
            0 => None,
            512 => Some(SIZE_512),
            8192 => Some(SIZE_8K),
            _ => return Err(StateError::Invalid("EEPROM size")),
        };
        let forced = r.bool()?;
        let rx = u128::from(r.u64()?) << 64 | u128::from(r.u64()?);
        let rx_len = r.u32()?;
        let tx = r.u64()?;
        let tx_left = r.u32()?;
        check(forced == self.forced, "EEPROM size option")?;
        check(!forced || size == self.size, "forced EEPROM size")?;
        // A request completes, and restarts, at 2 + n + 1 bits (read) or
        // 2 + n + 64 + 1 (write) for n address bits, so one still being received
        // is at most 2 + n + 64 bits long; n is 6 for a 512-byte chip and 14 for
        // the 8 KiB or still unknown one.
        let max_rx = if size == Some(SIZE_512) { 72 } else { 80 };
        check(rx_len <= max_rx && tx_left <= 68, "EEPROM transfer")?;
        *self = Self {
            data,
            size,
            forced,
            rx,
            rx_len,
            tx,
            tx_left,
        };
        Ok(())
    }

    /// Whether a save of `len` bytes fits this chip.
    pub fn accepts(&self, len: usize) -> bool {
        match (self.forced, self.size) {
            (true, Some(size)) => len == size,
            _ => len == SIZE_512 || len == SIZE_8K,
        }
    }

    /// The used part of the chip, once the size is known.
    pub fn data(&self) -> Option<&[u8]> {
        self.size.map(|s| &self.data[..s])
    }

    /// Install a loaded save (length already checked with [`Eeprom::accepts`]).
    pub fn load(&mut self, bytes: &[u8]) {
        self.data.fill(0xFF);
        self.data[..bytes.len()].copy_from_slice(bytes);
        // The file's size stands until a DMA length says otherwise.
        self.size = Some(bytes.len());
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
    /// request, and take the size from the transfer's length unless the size is
    /// forced. A recognised length wins over a loaded save's size: it is what the
    /// game itself uses, so a 512-byte game's save padded to 8 KiB must not pin
    /// 14-bit addressing. Shrinking erases the unused tail.
    pub fn begin_write_dma(&mut self, count: u32) {
        self.reset_serial();
        if self.forced {
            return;
        }
        let size = match count {
            9 | 73 => SIZE_512,
            17 | 81 => SIZE_8K,
            _ => return,
        };
        if size == SIZE_512 {
            self.data[SIZE_512..].fill(0xFF);
        }
        self.size = Some(size);
    }

    /// Shift one request bit in. Returns true if a write changed the stored data.
    pub fn write_bit(&mut self, bit: u16) -> bool {
        self.rx = (self.rx << 1) | u128::from(bit & 1);
        self.rx_len += 1;
        self.step()
    }

    /// Look at the request received so far and act if it is complete.
    fn step(&mut self) -> bool {
        let n = self.addr_bits();
        if self.rx_len == 2 && self.rx & 3 < 2 {
            // Not a request prefix (`10` / `11`): drop it.
            self.reset_serial();
            return false;
        }
        // 512 B uses 6 address bits; for 8 KiB the 14-bit field's low 10 select
        // one of the 1024 blocks and the top 4 are ignored.
        let amask: u128 = if n == 6 { 0x3F } else { 0x3FF };
        if self.rx_len == 2 + n + 1 && self.rx >> (n + 1) == 0b11 {
            self.settle_size();
            let addr = ((self.rx >> 1) & amask) as usize;
            self.tx = u64::from_be_bytes(self.data[addr * 8..addr * 8 + 8].try_into().unwrap());
            self.tx_left = 68;
            self.rx = 0;
            self.rx_len = 0;
        } else if self.rx_len == 2 + n + 64 + 1 {
            let prefix = self.rx >> (n + 65);
            let changed = if prefix == 0b10 {
                self.settle_size();
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

    /// A request completed while the size was unknown, so it was parsed as the
    /// 14-bit (8 KiB) form: that is the size.
    fn settle_size(&mut self) {
        if self.size.is_none() {
            self.size = Some(SIZE_8K);
        }
    }

    /// The game is about to read the chip back with the size still unknown and a
    /// request half-received: it is CPU-driven (no DMA length announced the
    /// size), so the bits received so far are the whole request. A length of 9 or
    /// 73 means 6-bit addresses (512 B), 17 or 81 means 14-bit (8 KiB), anything
    /// else defaults to 8 KiB. The pending request is then re-examined under the
    /// settled size. Returns true if that write changed the stored data.
    pub fn infer_pending(&mut self) -> bool {
        if self.size.is_some() || self.rx_len == 0 {
            return false;
        }
        self.size = Some(match self.rx_len {
            9 | 73 => SIZE_512,
            _ => SIZE_8K,
        });
        self.step()
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

    /// A request in flight is shorter than any request the chip's address width
    /// completes at; a longer count is a state no run produces, and on a 512-byte
    /// chip nothing would ever finish it.
    #[test]
    fn a_state_with_an_unfinishable_request_length_is_refused() {
        let load = |e: &Eeprom| {
            let mut w = StateWriter::new();
            e.save_state(&mut w);
            let bytes = w.into_bytes();
            let mut back = Eeprom::default();
            back.load_state(&mut StateReader::new(&bytes))
        };
        for (size, longest) in [(SIZE_512, 72), (SIZE_8K, 80)] {
            let mut e = Eeprom::default();
            e.begin_write_dma(if size == SIZE_512 { 9 } else { 17 });
            e.rx_len = longest;
            assert!(load(&e).is_ok(), "{size}: {longest}");
            e.rx_len = longest + 1;
            assert_eq!(
                load(&e),
                Err(StateError::Invalid("EEPROM transfer")),
                "{size}: {}",
                longest + 1
            );
        }
        // Unknown size: addressed as 8 KiB until told otherwise.
        let unknown = |rx_len| Eeprom {
            rx_len,
            ..Eeprom::default()
        };
        assert!(load(&unknown(81)).is_err());
        assert!(load(&unknown(80)).is_ok());
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
    fn a_dma_length_wins_over_a_loaded_size_of_the_other_chip() {
        // An 8 KiB file for a 512-byte game (a tool that pads every EEPROM save):
        // the game's 9- and 73-bit requests must still work.
        let mut e = Eeprom::default();
        let mut img = vec![0xFF; SIZE_8K];
        img[5 * 8..5 * 8 + 8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        img[0x3FF * 8..0x3FF * 8 + 8].copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7]);
        e.load(&img);
        assert_eq!(e.data().unwrap().len(), SIZE_8K, "the file's size, for now");
        e.begin_write_dma(9);
        assert_eq!(e.data().unwrap().len(), SIZE_512);
        send(&mut e, &read_req(5, 6));
        assert_eq!(read_value(&mut e), 0x0102_0304_0506_0708);
        assert!(send(&mut e, &write_req(7, 6, 0x1111_2222_3333_4444)));
        send(&mut e, &read_req(7, 6));
        assert_eq!(read_value(&mut e), 0x1111_2222_3333_4444);
        // The padding is gone, not kept behind the 512 bytes.
        assert!(e.data.iter().skip(SIZE_512).all(|&b| b == 0xFF));
        // And the other way: an 8 KiB game's 17-bit request restores 14-bit
        // addressing, over a 512-byte file.
        let mut f = Eeprom::default();
        f.load(&img[..SIZE_512]);
        f.begin_write_dma(17);
        assert_eq!(f.data().unwrap().len(), SIZE_8K);
    }

    /// Bits stored one at a time by the CPU: no DMA announces the length.
    fn send_cpu(e: &mut Eeprom, bits: &[u8]) -> bool {
        bits.iter()
            .fold(false, |c, &b| e.write_bit(u16::from(b)) | c)
    }

    #[test]
    fn cpu_driven_512_is_sized_from_the_request_length_at_the_first_read() {
        let mut e = Eeprom::default();
        // A 73-bit write is not complete under the 14-bit assumption, so nothing
        // is stored and the size is still open...
        assert!(!send_cpu(
            &mut e,
            &write_req(0x2A, 6, 0x0123_4567_89AB_CDEF)
        ));
        assert!(e.data().is_none());
        // ...until the ready poll: 73 bits means 6-bit addressing.
        assert!(e.infer_pending());
        assert_eq!(e.data().unwrap().len(), SIZE_512);
        assert_eq!(e.read_bit(), 1, "ready");
        send_cpu(&mut e, &read_req(0x2A, 6));
        assert!(!e.infer_pending());
        assert_eq!(read_value(&mut e), 0x0123_4567_89AB_CDEF);
    }

    #[test]
    fn cpu_driven_read_request_of_512_starts_at_the_first_read() {
        let mut e = Eeprom::default();
        send_cpu(&mut e, &write_req(3, 6, 99));
        e.infer_pending();
        send_cpu(&mut e, &read_req(3, 6));
        e.infer_pending();
        assert_eq!(read_value(&mut e), 99);
        assert_eq!(e.data().unwrap().len(), SIZE_512);
    }

    #[test]
    fn cpu_driven_8k_completes_under_the_default_assumption() {
        let mut e = Eeprom::default();
        assert!(send_cpu(&mut e, &write_req(0x3FF, 14, 42)));
        assert_eq!(
            e.data().unwrap().len(),
            SIZE_8K,
            "an 81-bit request is 8 KiB"
        );
        assert!(!e.infer_pending());
        send_cpu(&mut e, &read_req(0x3FF, 14));
        assert_eq!(read_value(&mut e), 42);
        // A 17-bit read request on a fresh chip sizes it the same way.
        let mut f = Eeprom::default();
        send_cpu(&mut f, &read_req(5, 14));
        assert_eq!(f.data().unwrap().len(), SIZE_8K);
    }

    #[test]
    fn an_unrecognised_length_defaults_to_8k_and_no_request_infers_nothing() {
        let mut e = Eeprom::default();
        assert!(!e.infer_pending(), "an idle poll decides nothing");
        assert!(e.data().is_none());
        send_cpu(&mut e, &[1, 0, 1, 0, 1]);
        e.infer_pending();
        assert_eq!(e.data().unwrap().len(), SIZE_8K);
    }

    #[test]
    fn a_forced_size_is_exclusive() {
        let mut e = Eeprom::with_size(SIZE_512);
        assert_eq!(e.data().unwrap().len(), SIZE_512);
        assert!(e.accepts(SIZE_512) && !e.accepts(SIZE_8K));
        // A DMA length cannot change it.
        e.begin_write_dma(81);
        assert_eq!(e.data().unwrap().len(), SIZE_512);
        let e = Eeprom::with_size(SIZE_8K);
        assert!(e.accepts(SIZE_8K) && !e.accepts(SIZE_512));
        let free = Eeprom::default();
        assert!(free.accepts(SIZE_512) && free.accepts(SIZE_8K) && !free.accepts(100));
    }
}
