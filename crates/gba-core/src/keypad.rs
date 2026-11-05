//! Keypad: KEYINPUT (0x0400_0130, active-low) and KEYCNT (0x0400_0132). Owned by
//! P1-SYS; [`Button`] is a **Contract**.

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

impl Button {
    pub const ALL: [Button; 10] = [
        Button::A,
        Button::B,
        Button::Select,
        Button::Start,
        Button::Right,
        Button::Left,
        Button::Up,
        Button::Down,
        Button::R,
        Button::L,
    ];
}

#[derive(Debug, Clone, Default)]
pub struct Keypad {
    /// Bit n set = button n held.
    held: u16,
    /// KEYCNT.
    pub keycnt: u16,
}

impl Keypad {
    pub fn set(&mut self, b: Button, pressed: bool) {
        let bit = 1u16 << b as u16;
        if pressed {
            self.held |= bit;
        } else {
            self.held &= !bit;
        }
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
}
