//! Interrupt flag (IF, 0xFF0F) and enable (IE, 0xFFFF) registers.
//!
//! Pan Docs: "Interrupts" / "Interrupt Sources". Components request an interrupt by
//! setting its IF bit via [`Interrupts::request`]; the CPU checks
//! [`Interrupts::pending`] and acknowledges (clears) the bit it services.

/// One interrupt source. The discriminant is its bit mask in IE/IF; priority
/// is lowest bit first (VBlank highest).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Interrupt {
    VBlank = 0x01,
    Stat = 0x02,
    Timer = 0x04,
    Serial = 0x08,
    Joypad = 0x10,
}

impl Interrupt {
    /// Bit mask in IE/IF.
    pub const fn mask(self) -> u8 {
        self as u8
    }

    /// Handler address the CPU jumps to (0x40, 0x48, 0x50, 0x58, 0x60).
    pub const fn vector(self) -> u16 {
        match self {
            Interrupt::VBlank => 0x40,
            Interrupt::Stat => 0x48,
            Interrupt::Timer => 0x50,
            Interrupt::Serial => 0x58,
            Interrupt::Joypad => 0x60,
        }
    }

    /// Highest-priority interrupt whose bit is set in `mask`, if any.
    pub const fn highest_priority(mask: u8) -> Option<Interrupt> {
        let m = mask & 0x1F;
        if m == 0 {
            return None;
        }
        Some(match m.trailing_zeros() {
            0 => Interrupt::VBlank,
            1 => Interrupt::Stat,
            2 => Interrupt::Timer,
            3 => Interrupt::Serial,
            _ => Interrupt::Joypad,
        })
    }
}

/// IE and IF register state.
#[derive(Clone, Debug, Default)]
pub struct Interrupts {
    /// IE (0xFFFF). All 8 bits are readable/writable on DMG.
    pub enable: u8,
    /// IF (0xFF0F), low 5 bits only. Upper 3 bits read back as 1.
    pub flags: u8,
}

impl Interrupts {
    /// Post-boot DMG state: IF = 0xE1 (VBlank pending), IE = 0x00.
    pub fn new() -> Self {
        Self {
            enable: 0x00,
            flags: 0x01,
        }
    }

    pub fn request(&mut self, interrupt: Interrupt) {
        self.flags |= interrupt.mask();
    }

    /// Clear one IF bit (used by the CPU when dispatching).
    pub fn acknowledge(&mut self, mask: u8) {
        self.flags &= !mask;
    }

    /// `IE & IF` restricted to the 5 real sources. Nonzero wakes HALT even with IME=0.
    pub fn pending(&self) -> u8 {
        self.enable & self.flags & 0x1F
    }

    pub fn read_if(&self) -> u8 {
        self.flags | 0xE0
    }

    pub fn write_if(&mut self, value: u8) {
        self.flags = value & 0x1F;
    }

    pub fn read_ie(&self) -> u8 {
        self.enable
    }

    pub fn write_ie(&mut self, value: u8) {
        self.enable = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_is_lowest_bit_first() {
        assert_eq!(Interrupt::highest_priority(0), None);
        assert_eq!(Interrupt::highest_priority(0b10110), Some(Interrupt::Stat));
        assert_eq!(Interrupt::highest_priority(0xE0), None);
        assert_eq!(Interrupt::highest_priority(0x10), Some(Interrupt::Joypad));
    }

    #[test]
    fn if_upper_bits_read_as_one() {
        let mut i = Interrupts::new();
        i.write_if(0xFF);
        assert_eq!(i.read_if(), 0xFF);
        i.write_if(0x00);
        assert_eq!(i.read_if(), 0xE0);
    }

    #[test]
    fn pending_masks_with_ie() {
        let mut i = Interrupts::new();
        i.write_if(0x1F);
        i.write_ie(0x04);
        assert_eq!(i.pending(), 0x04);
    }
}
