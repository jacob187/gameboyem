//! Joypad register P1/JOYP (Pan Docs: "Joypad Input").

use crate::interrupts::{Interrupt, Interrupts};
use crate::state::{StateError, StateReader, StateWriter, check};

/// A DMG button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Button {
    Right,
    Left,
    Up,
    Down,
    A,
    B,
    Select,
    Start,
}

impl Button {
    pub const ALL: [Button; 8] = [
        Button::Right,
        Button::Left,
        Button::Up,
        Button::Down,
        Button::A,
        Button::B,
        Button::Select,
        Button::Start,
    ];

    /// The Game Boy button for a cross-console one; `None` for the GBA-only
    /// `L` and `R`. Index `i` of [`Button::ALL`] is index `i` of
    /// `emu_common::Button::ALL`.
    pub const fn from_common(button: emu_common::Button) -> Option<Self> {
        Some(match button {
            emu_common::Button::Right => Button::Right,
            emu_common::Button::Left => Button::Left,
            emu_common::Button::Up => Button::Up,
            emu_common::Button::Down => Button::Down,
            emu_common::Button::A => Button::A,
            emu_common::Button::B => Button::B,
            emu_common::Button::Select => Button::Select,
            emu_common::Button::Start => Button::Start,
            emu_common::Button::L | emu_common::Button::R => return None,
        })
    }

    /// (is_action_button, bit in the low nibble)
    const fn line(self) -> (bool, u8) {
        match self {
            Button::Right => (false, 0),
            Button::Left => (false, 1),
            Button::Up => (false, 2),
            Button::Down => (false, 3),
            Button::A => (true, 0),
            Button::B => (true, 1),
            Button::Select => (true, 2),
            Button::Start => (true, 3),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Joypad {
    /// P1 bits 4-5 as last written (0 = group selected).
    select: u8,
    /// Pressed d-pad buttons, bit set = pressed.
    dpad: u8,
    /// Pressed action buttons, bit set = pressed.
    action: u8,
}

impl Joypad {
    /// Post-boot P1 reads 0xCF (Pan Docs "Power Up Sequence"): both groups selected.
    pub fn new() -> Self {
        Self {
            select: 0x00,
            dpad: 0,
            action: 0,
        }
    }

    /// CGB post-boot P1. In DMG-compatibility mode no group is selected (reads
    /// 0xFF, mooneye `boot_hwio-C`); in CGB mode both are, as on a DMG (Pan
    /// Docs lists 0xC7 / 0xCF; the choice of 0xCF is DECISIONS #34).
    pub fn new_cgb(compat: bool) -> Self {
        Self {
            select: if compat { 0x30 } else { 0x00 },
            ..Self::new()
        }
    }

    fn low_nibble(&self) -> u8 {
        let mut pressed = 0;
        if self.select & 0x10 == 0 {
            pressed |= self.dpad;
        }
        if self.select & 0x20 == 0 {
            pressed |= self.action;
        }
        !pressed & 0x0F
    }

    /// True while a selected input line is low; wakes the CPU from STOP.
    pub fn any_selected_low(&self) -> bool {
        self.low_nibble() != 0x0F
    }

    pub fn read(&self) -> u8 {
        0xC0 | self.select | self.low_nibble()
    }

    /// Selecting a group whose button is held pulls a line low, which is a
    /// falling edge and requests the joypad interrupt like a key press.
    pub fn write(&mut self, value: u8, interrupts: &mut Interrupts) {
        let before = self.low_nibble();
        self.select = value & 0x30;
        if before & !self.low_nibble() != 0 {
            interrupts.request(Interrupt::Joypad);
        }
    }

    /// Update a button. Requests the joypad interrupt when a selected line goes low.
    pub fn set_button(&mut self, button: Button, pressed: bool, interrupts: &mut Interrupts) {
        let before = self.low_nibble();
        let (action, bit) = button.line();
        let group = if action {
            &mut self.action
        } else {
            &mut self.dpad
        };
        if pressed {
            *group |= 1 << bit;
        } else {
            *group &= !(1 << bit);
        }
        if before & !self.low_nibble() != 0 {
            interrupts.request(Interrupt::Joypad);
        }
    }
}

impl Joypad {
    pub(crate) fn save_state(&self, w: &mut StateWriter) {
        w.u8(self.select);
        w.u8(self.dpad);
        w.u8(self.action);
    }

    pub(crate) fn load_state(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let j = Self {
            select: r.u8()?,
            dpad: r.u8()?,
            action: r.u8()?,
        };
        check(j.select & !0x30 == 0, "joypad select")?;
        check(j.dpad <= 0x0F && j.action <= 0x0F, "joypad buttons")?;
        *self = j;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recordings store a button as its index in `ALL`, so the two lists must
    /// agree for every Game Boy button, and `L` / `R` must not map.
    #[test]
    fn button_order_matches_the_cross_console_type() {
        for (i, b) in Button::ALL.into_iter().enumerate() {
            assert_eq!(Button::from_common(emu_common::Button::ALL[i]), Some(b));
        }
        assert_eq!(Button::from_common(emu_common::Button::L), None);
        assert_eq!(Button::from_common(emu_common::Button::R), None);
    }

    #[test]
    fn selected_group_reads_active_low() {
        let mut j = Joypad::new();
        let mut i = Interrupts::new();
        i.write_if(0);
        assert_eq!(j.read(), 0xCF, "post-boot P1");
        j.write(0x30, &mut i); // deselect both groups
        j.set_button(Button::Start, true, &mut i);
        assert_eq!(i.read_if() & 0x10, 0, "no interrupt when group unselected");
        j.write(0x10, &mut i); // select action buttons: Start's line falls
        assert_eq!(j.read() & 0x0F, 0b0111);
        assert_eq!(
            i.read_if() & 0x10,
            0x10,
            "selecting a held button is an edge"
        );
        i.write_if(0);
        j.write(0x20, &mut i); // select d-pad
        assert_eq!(j.read() & 0x0F, 0x0F);
        j.set_button(Button::Down, true, &mut i);
        assert_eq!(j.read() & 0x0F, 0b0111);
        assert_eq!(i.read_if() & 0x10, 0x10);
    }
}
