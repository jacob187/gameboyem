//! Flash backup chips (64 KiB and 128 KiB). GBATEK "GBA Cart Backup Flash ROM".
//!
//! The chip sits behind the 16-bit "SRAM" address bus (0x0E00_0000..0x0E00_FFFF),
//! so a 128 KiB part is two 64 KiB banks selected by a command. Commands are
//! written byte-wise: `0xAA` at 0x5555, `0x55` at 0x2AAA, then a command byte.
//! Erase and program complete instantly (the "wait until" polls in software see
//! the finished state on the first read); real chips take milliseconds.

/// Manufacturer / device IDs reported in ID mode, from GBATEK's device table.
///
/// 64 KiB: Panasonic MN63F805MNP (ID 0x1B32). 128 KiB: Sanyo LE26FV10N1TS (ID
/// 0x1362). Both are on the list of commonly emulated chips (DECISIONS #53).
use crate::state::{StateError, check};
use emu_common::state::{StateReader, StateWriter};

const ID_64K: [u8; 2] = [0x32, 0x1B];
const ID_128K: [u8; 2] = [0x62, 0x13];

pub const SIZE_64K: usize = 64 * 1024;
pub const SIZE_128K: usize = 128 * 1024;
const SECTOR: usize = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// Got `0xAA` at 0x5555.
    Unlocked1,
    /// Got `0x55` at 0x2AAA too: the next write is a command.
    Unlocked2,
    /// `0xA0` was given: the next write programs one byte.
    Program,
    /// `0xB0` was given (128 KiB only): the next write to 0x0000 selects the bank.
    BankSelect,
}

pub struct Flash {
    data: Box<[u8]>,
    state: State,
    id_mode: bool,
    /// `0x80` was given: the next unlocked `0x10` / `0x30` erases.
    erase_armed: bool,
    bank: usize,
}

impl Flash {
    pub fn new(size: usize) -> Self {
        debug_assert!(size == SIZE_64K || size == SIZE_128K);
        Self {
            data: vec![0xFF; size].into_boxed_slice(),
            state: State::Idle,
            id_mode: false,
            erase_armed: false,
            bank: 0,
        }
    }

    /// An erased chip of the same size.
    pub fn blank_like(&self) -> Self {
        Self::new(self.data.len())
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// The whole array, the command state machine (so a write sequence can be
    /// interrupted between its bytes), ID mode, the armed erase and the bank.
    pub fn save_state(&self, w: &mut StateWriter) {
        w.bytes(&self.data);
        w.u8(match self.state {
            State::Idle => 0,
            State::Unlocked1 => 1,
            State::Unlocked2 => 2,
            State::Program => 3,
            State::BankSelect => 4,
        });
        w.bool(self.id_mode);
        w.bool(self.erase_armed);
        w.u8(self.bank as u8);
    }

    /// The chip's size comes from the cartridge (the save type), not the file.
    /// A 64 KiB chip has one bank and no bank-select command: a bank of 1 would
    /// index past its array.
    pub fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let mut data = vec![0; self.data.len()].into_boxed_slice();
        r.bytes_into(&mut data)?;
        let state = match r.u8()? {
            0 => State::Idle,
            1 => State::Unlocked1,
            2 => State::Unlocked2,
            3 => State::Program,
            4 => State::BankSelect,
            _ => return Err(StateError::Invalid("flash state")),
        };
        let (id_mode, erase_armed) = (r.bool()?, r.bool()?);
        let bank = usize::from(r.u8()?);
        let two_banks = data.len() == SIZE_128K;
        check(bank <= usize::from(two_banks), "flash bank")?;
        check(two_banks || state != State::BankSelect, "flash bank select")?;
        *self = Self {
            data,
            state,
            id_mode,
            erase_armed,
            bank,
        };
        Ok(())
    }

    /// Replace the contents (size already validated) and reset the command state.
    pub fn load(&mut self, bytes: &[u8]) {
        self.data.copy_from_slice(bytes);
        self.state = State::Idle;
        self.id_mode = false;
        self.erase_armed = false;
        self.bank = 0;
    }

    fn id(&self) -> [u8; 2] {
        if self.data.len() == SIZE_128K {
            ID_128K
        } else {
            ID_64K
        }
    }

    /// Read the byte at `addr` (0..0x10000 within the current bank).
    pub fn read(&self, addr: u32) -> u8 {
        let a = addr as usize & 0xFFFF;
        if self.id_mode && a < 2 {
            return self.id()[a];
        }
        self.data[self.bank * SIZE_64K + a]
    }

    /// Write a byte; returns true if the stored data changed.
    pub fn write(&mut self, addr: u32, v: u8) -> bool {
        let a = addr as usize & 0xFFFF;
        match self.state {
            State::Program => {
                self.state = State::Idle;
                let cell = &mut self.data[self.bank * SIZE_64K + a];
                // Programming can only clear bits; erase sets them (GBATEK).
                let new = *cell & v;
                let changed = new != *cell;
                *cell = new;
                changed
            }
            State::BankSelect => {
                self.state = State::Idle;
                if a == 0 {
                    self.bank = usize::from(v & 1);
                }
                false
            }
            State::Idle => {
                if a == 0x5555 && v == 0xAA {
                    self.state = State::Unlocked1;
                } else if v == 0xF0 {
                    // Reset / terminate ID mode without the unlock sequence.
                    self.id_mode = false;
                }
                false
            }
            State::Unlocked1 => {
                if a == 0x2AAA && v == 0x55 {
                    self.state = State::Unlocked2;
                    false
                } else {
                    // Not the second unlock byte: look at it as a fresh start, so
                    // a repeated 0xAA@0x5555 keeps the unlock going (code-review S#4).
                    self.state = State::Idle;
                    self.write(addr, v)
                }
            }
            State::Unlocked2 => {
                self.state = State::Idle;
                self.command(a, v)
            }
        }
    }

    fn command(&mut self, a: usize, v: u8) -> bool {
        let armed = std::mem::take(&mut self.erase_armed);
        if a == 0x5555 {
            match v {
                0x90 => self.id_mode = true,
                0xF0 => self.id_mode = false,
                0x80 => self.erase_armed = true,
                0xA0 => self.state = State::Program,
                0xB0 if self.data.len() == SIZE_128K => self.state = State::BankSelect,
                0x10 if armed => return fill_ff(&mut self.data),
                _ => {}
            }
            false
        } else if v == 0x30 && armed {
            let base = self.bank * SIZE_64K + (a & 0xF000);
            fill_ff(&mut self.data[base..base + SECTOR])
        } else {
            false
        }
    }
}

/// Erase `s` to 0xFF; true if anything changed.
fn fill_ff(s: &mut [u8]) -> bool {
    let changed = s.iter().any(|&b| b != 0xFF);
    s.fill(0xFF);
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(f: &mut Flash, c: u8) {
        f.write(0x5555, 0xAA);
        f.write(0x2AAA, 0x55);
        f.write(0x5555, c);
    }

    fn program(f: &mut Flash, addr: u32, v: u8) {
        cmd(f, 0xA0);
        f.write(addr, v);
    }

    #[test]
    fn starts_erased_and_programs_bytes() {
        let mut f = Flash::new(SIZE_64K);
        assert_eq!(f.read(0x1234), 0xFF);
        // A plain write without the command is ignored.
        f.write(0x1234, 0x11);
        assert_eq!(f.read(0x1234), 0xFF);
        program(&mut f, 0x1234, 0x42);
        assert_eq!(f.read(0x1234), 0x42);
    }

    #[test]
    fn id_mode_reports_chip_and_exits() {
        let mut f = Flash::new(SIZE_64K);
        cmd(&mut f, 0x90);
        assert_eq!((f.read(0), f.read(1)), (0x32, 0x1B));
        cmd(&mut f, 0xF0);
        assert_eq!(f.read(0), 0xFF);
        let mut g = Flash::new(SIZE_128K);
        cmd(&mut g, 0x90);
        assert_eq!((g.read(0), g.read(1)), (0x62, 0x13));
    }

    #[test]
    fn sector_and_chip_erase() {
        let mut f = Flash::new(SIZE_64K);
        program(&mut f, 0x0000, 0);
        program(&mut f, 0x0FFF, 0);
        program(&mut f, 0x1000, 0);
        cmd(&mut f, 0x80);
        f.write(0x5555, 0xAA);
        f.write(0x2AAA, 0x55);
        f.write(0x0000, 0x30);
        assert_eq!((f.read(0), f.read(0xFFF), f.read(0x1000)), (0xFF, 0xFF, 0));
        cmd(&mut f, 0x80);
        cmd(&mut f, 0x10);
        assert_eq!(f.read(0x1000), 0xFF);
    }

    #[test]
    fn a_retried_unlock_sequence_still_works() {
        let mut f = Flash::new(SIZE_64K);
        f.write(0x5555, 0xAA);
        f.write(0x5555, 0xAA); // restart, not abort
        f.write(0x2AAA, 0x55);
        f.write(0x5555, 0xA0);
        f.write(0x10, 0x42);
        assert_eq!(f.read(0x10), 0x42);
        // A junk byte in between aborts the sequence.
        f.write(0x5555, 0xAA);
        f.write(0x1234, 0x00);
        f.write(0x2AAA, 0x55);
        f.write(0x5555, 0xA0);
        f.write(0x11, 0x42);
        assert_eq!(f.read(0x11), 0xFF);
    }

    #[test]
    fn erase_needs_the_arming_command() {
        let mut f = Flash::new(SIZE_64K);
        program(&mut f, 0, 0);
        cmd(&mut f, 0x10);
        assert_eq!(f.read(0), 0);
    }

    #[test]
    fn bank_switch_on_128k_only() {
        let mut f = Flash::new(SIZE_128K);
        program(&mut f, 0x10, 0x11);
        cmd(&mut f, 0xB0);
        f.write(0, 1);
        assert_eq!(f.read(0x10), 0xFF);
        program(&mut f, 0x10, 0x22);
        cmd(&mut f, 0xB0);
        f.write(0, 0);
        assert_eq!(f.read(0x10), 0x11);
        let mut g = Flash::new(SIZE_64K);
        cmd(&mut g, 0xB0);
        g.write(0, 1);
        program(&mut g, 0x10, 0x33);
        assert_eq!(g.read(0x10), 0x33);
    }

    #[test]
    fn programming_only_clears_bits() {
        let mut f = Flash::new(SIZE_64K);
        program(&mut f, 5, 0xF0);
        program(&mut f, 5, 0x3C);
        assert_eq!(f.read(5), 0x30);
    }

    #[test]
    fn write_reports_changes() {
        let mut f = Flash::new(SIZE_64K);
        cmd(&mut f, 0xA0);
        assert!(f.write(0, 0x00));
        cmd(&mut f, 0xA0);
        assert!(!f.write(0, 0x00));
    }
}
