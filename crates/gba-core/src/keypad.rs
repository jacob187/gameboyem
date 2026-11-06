//! Keypad: KEYINPUT (0x0400_0130, active-low) and KEYCNT (0x0400_0132). Owned by
//! P1-SYS; [`Button`] is a **Contract**.

use crate::state::{StateError, check};
use emu_common::state::{StateReader, StateWriter};

/// The ten GBA buttons, in KEYINPUT bit order (bit 0 = A ... bit 9 = L).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    A = 0,
    B = 1,
    Select = 2,
    Start = 3,
    Right = 4,
    Left = 5,
    Up = 6,
    Down = 7,
    R = 8,
    L = 9,
}

#[derive(Debug, Clone, Default)]
pub struct Keypad {
    /// Bit n set = button n held.
    held: u16,
    /// KEYCNT (0x0400_0132): bits 0-9 button mask, 14 IRQ enable, 15 AND mode.
    pub keycnt: u16,
    /// Whether the interrupt condition held at the last check, so the IRQ fires
    /// on the rising edge only.
    irq_level: bool,
}

/// KEYCNT's writable bits.
pub const KEYCNT_MASK: u16 = 0xC3FF;

impl Keypad {
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.u16(self.held);
        w.u16(self.keycnt);
        w.bool(self.irq_level);
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let (held, keycnt, irq_level) = (r.u16()?, r.u16()?, r.bool()?);
        check(held <= 0x3FF, "held keys")?;
        check(keycnt & !KEYCNT_MASK == 0, "KEYCNT")?;
        *self = Self {
            held,
            keycnt,
            irq_level,
        };
        Ok(())
    }

    pub fn set(&mut self, b: Button, pressed: bool) {
        let bit = 1u16 << b as u16;
        if pressed {
            self.held |= bit;
        } else {
            self.held &= !bit;
        }
    }

    /// Write KEYCNT through a byte-lane `mask`.
    pub fn write_keycnt(&mut self, v: u16, mask: u16) {
        let mask = mask & KEYCNT_MASK;
        self.keycnt = (self.keycnt & !mask) | (v & mask);
    }

    /// Re-evaluate the keypad interrupt condition (GBATEK "GBA Keypad Input",
    /// KEYCNT): IRQ enabled and either any (OR mode) or all (AND mode) of the
    /// selected buttons held. Returns true when the condition just became true,
    /// i.e. an IF.Keypad flag should be raised.
    pub fn irq_edge(&mut self) -> bool {
        let sel = self.keycnt & 0x03FF;
        let held = self.held & sel;
        let cond = self.keycnt & 0x4000 != 0
            && if self.keycnt & 0x8000 != 0 {
                // AND mode with nothing selected never fires.
                sel != 0 && held == sel
            } else {
                held != 0
            };
        let edge = cond && !self.irq_level;
        self.irq_level = cond;
        edge
    }

    /// KEYINPUT: 0 = pressed.
    pub fn keyinput(&self) -> u16 {
        !self.held & 0x03FF
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyinput_is_active_low() {
        let mut k = Keypad::default();
        assert_eq!(k.keyinput(), 0x3FF);
        k.set(Button::L, true);
        k.set(Button::A, true);
        assert_eq!(k.keyinput(), 0x1FE);
        k.set(Button::A, false);
        assert_eq!(k.keyinput(), 0x1FF);
    }

    #[test]
    fn or_mode_fires_on_the_rising_edge_only() {
        let mut k = Keypad::default();
        k.write_keycnt(0x4000 | 0b11, 0xFFFF);
        assert!(!k.irq_edge());
        k.set(Button::B, true);
        assert!(k.irq_edge());
        k.set(Button::A, true);
        assert!(!k.irq_edge(), "already true");
        k.set(Button::A, false);
        k.set(Button::B, false);
        assert!(!k.irq_edge());
        k.set(Button::A, true);
        assert!(k.irq_edge());
    }

    #[test]
    fn and_mode_needs_every_selected_button() {
        let mut k = Keypad::default();
        k.write_keycnt(0xC000 | 0b101, 0xFFFF);
        k.set(Button::A, true);
        assert!(!k.irq_edge());
        k.set(Button::Select, true);
        assert!(k.irq_edge());
        // An empty AND selection never fires.
        k.write_keycnt(0xC000, 0xFFFF);
        assert!(!k.irq_edge());
    }

    #[test]
    fn enabling_irq_with_buttons_already_held_fires() {
        let mut k = Keypad::default();
        k.set(Button::Start, true);
        k.write_keycnt(1 << 3, 0xFFFF);
        assert!(!k.irq_edge(), "IRQ enable bit is clear");
        k.write_keycnt(0x4000 | 1 << 3, 0xFFFF);
        assert!(k.irq_edge());
    }

    #[test]
    fn keycnt_only_keeps_defined_bits() {
        let mut k = Keypad::default();
        k.write_keycnt(0xFFFF, 0xFFFF);
        assert_eq!(k.keycnt, 0xC3FF);
    }
}
