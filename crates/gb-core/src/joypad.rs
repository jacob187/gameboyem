//! Joypad register P1/JOYP (Pan Docs: "Joypad Input").

use crate::interrupts::{Interrupt, Interrupts};

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

    pub fn write(&mut self, value: u8) {
        self.select = value & 0x30;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_group_reads_active_low() {
        let mut j = Joypad::new();
        let mut i = Interrupts::new();
        i.write_if(0);
        assert_eq!(j.read(), 0xCF, "post-boot P1");
        j.write(0x30); // deselect both groups
        j.set_button(Button::Start, true, &mut i);
        assert_eq!(i.read_if() & 0x10, 0, "no interrupt when group unselected");
        j.write(0x10); // select action buttons
        assert_eq!(j.read() & 0x0F, 0b0111);
        j.write(0x20); // select d-pad
        assert_eq!(j.read() & 0x0F, 0x0F);
        j.set_button(Button::Down, true, &mut i);
        assert_eq!(j.read() & 0x0F, 0b0111);
        assert_eq!(i.read_if() & 0x10, 0x10);
    }
}
