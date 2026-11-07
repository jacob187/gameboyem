//! Keyboard -> emulator actions. Keys are matched by physical position
//! (`KeyCode`), so Z/X stay put on non-QWERTY layouts.

use std::collections::HashSet;

use emu_common::Button;
use winit::keyboard::KeyCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Button(Button),
    Quit,
    Pause,
    FastForward,
    SaveState,
    LoadState,
    /// Choose save-state slot 1..=9.
    SelectSlot(u8),
    /// Save the current picture as a PNG (and mark the frame in a recording).
    Screenshot,
}

/// Printed once when the window opens on a Game Boy, where S used to save a state.
pub const SAVE_KEY_NOTICE: &str = "save state is K, load is L (S is now D-pad Down)";

pub fn map(key: KeyCode) -> Option<Action> {
    Some(match key {
        KeyCode::ArrowUp | KeyCode::KeyW => Action::Button(Button::Up),
        KeyCode::ArrowDown | KeyCode::KeyS => Action::Button(Button::Down),
        KeyCode::ArrowLeft | KeyCode::KeyA => Action::Button(Button::Left),
        KeyCode::ArrowRight | KeyCode::KeyD => Action::Button(Button::Right),
        KeyCode::KeyX | KeyCode::KeyE => Action::Button(Button::A),
        KeyCode::KeyZ => Action::Button(Button::B),
        KeyCode::Enter | KeyCode::NumpadEnter => Action::Button(Button::Start),
        KeyCode::Backspace | KeyCode::ShiftRight => Action::Button(Button::Select),
        // GBA shoulders; a Game Boy has neither and the app drops them.
        KeyCode::KeyQ => Action::Button(Button::L),
        KeyCode::KeyR => Action::Button(Button::R),
        KeyCode::Escape => Action::Quit,
        KeyCode::KeyP => Action::Pause,
        KeyCode::Tab => Action::FastForward,
        KeyCode::KeyK => Action::SaveState,
        KeyCode::KeyL => Action::LoadState,
        KeyCode::Digit1 => Action::SelectSlot(1),
        KeyCode::Digit2 => Action::SelectSlot(2),
        KeyCode::Digit3 => Action::SelectSlot(3),
        KeyCode::Digit4 => Action::SelectSlot(4),
        KeyCode::Digit5 => Action::SelectSlot(5),
        KeyCode::Digit6 => Action::SelectSlot(6),
        KeyCode::Digit7 => Action::SelectSlot(7),
        KeyCode::Digit8 => Action::SelectSlot(8),
        KeyCode::Digit9 => Action::SelectSlot(9),
        KeyCode::KeyC => Action::Screenshot,
        _ => return None,
    })
}

/// Tracks held keys so that two keys bound to one button (Backspace and Right
/// Shift) only release it when both are up.
#[derive(Default)]
pub struct InputState {
    held: HashSet<KeyCode>,
}

impl InputState {
    pub fn is_down(&self, button: Button) -> bool {
        self.held
            .iter()
            .any(|&k| map(k) == Some(Action::Button(button)))
    }

    /// Record a key event. Returns `(button, pressed)` only when the aggregate
    /// state of a joypad button actually changed.
    pub fn key(&mut self, key: KeyCode, pressed: bool) -> Option<(Button, bool)> {
        let Some(Action::Button(button)) = map(key) else {
            return None;
        };
        let before = self.is_down(button);
        if pressed {
            self.held.insert(key);
        } else {
            self.held.remove(&key);
        }
        let after = self.is_down(button);
        (before != after).then_some((button, after))
    }

    /// Forget every held key (window lost focus); returns buttons to release.
    pub fn release_all(&mut self) -> Vec<Button> {
        let down: Vec<Button> = Button::ALL
            .into_iter()
            .filter(|&b| self.is_down(b))
            .collect();
        self.held.clear();
        down
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bindings() {
        assert_eq!(map(KeyCode::KeyX), Some(Action::Button(Button::A)));
        assert_eq!(map(KeyCode::KeyZ), Some(Action::Button(Button::B)));
        assert_eq!(map(KeyCode::Enter), Some(Action::Button(Button::Start)));
        assert_eq!(
            map(KeyCode::Backspace),
            Some(Action::Button(Button::Select))
        );
        assert_eq!(
            map(KeyCode::ShiftRight),
            Some(Action::Button(Button::Select))
        );
        assert_eq!(map(KeyCode::ArrowUp), Some(Action::Button(Button::Up)));
        assert_eq!(map(KeyCode::Escape), Some(Action::Quit));
        assert_eq!(map(KeyCode::KeyP), Some(Action::Pause));
        assert_eq!(map(KeyCode::Tab), Some(Action::FastForward));
        assert_eq!(map(KeyCode::KeyK), Some(Action::SaveState));
        assert_eq!(map(KeyCode::KeyL), Some(Action::LoadState));
        assert_eq!(map(KeyCode::Digit1), Some(Action::SelectSlot(1)));
        assert_eq!(map(KeyCode::Digit9), Some(Action::SelectSlot(9)));
        assert_eq!(map(KeyCode::Digit0), None);
        assert_eq!(map(KeyCode::KeyC), Some(Action::Screenshot));
        assert_eq!(map(KeyCode::KeyJ), None);
    }

    #[test]
    fn every_non_button_action_has_its_own_key() {
        // Screenshot (C) must not collide with save states (K, L, 1-9) or the
        // joypad: each of these keys maps to a different action.
        let keys = [
            KeyCode::KeyC,
            KeyCode::KeyK,
            KeyCode::KeyL,
            KeyCode::KeyP,
            KeyCode::Tab,
            KeyCode::Escape,
        ]
        .into_iter()
        .chain([
            KeyCode::Digit1,
            KeyCode::Digit2,
            KeyCode::Digit3,
            KeyCode::Digit4,
            KeyCode::Digit5,
            KeyCode::Digit6,
            KeyCode::Digit7,
            KeyCode::Digit8,
            KeyCode::Digit9,
        ]);
        let mut seen = Vec::new();
        for key in keys {
            let action = map(key).expect("bound");
            assert!(!seen.contains(&action), "{action:?} is bound twice");
            seen.push(action);
        }
        assert_eq!(map(KeyCode::KeyC), Some(Action::Screenshot));
        let mut s = InputState::default();
        for key in [KeyCode::KeyC, KeyCode::KeyK, KeyCode::KeyL, KeyCode::Digit1] {
            assert_eq!(s.key(key, true), None, "{key:?} must not move a button");
        }
    }

    #[test]
    fn all_eight_buttons_are_reachable() {
        let mapped: HashSet<Button> = [
            KeyCode::ArrowUp,
            KeyCode::ArrowDown,
            KeyCode::ArrowLeft,
            KeyCode::ArrowRight,
            KeyCode::KeyX,
            KeyCode::KeyZ,
            KeyCode::Enter,
            KeyCode::Backspace,
        ]
        .into_iter()
        .filter_map(|k| match map(k) {
            Some(Action::Button(b)) => Some(b),
            _ => None,
        })
        .collect();
        assert_eq!(mapped.len(), 8);
    }

    #[test]
    fn wasd_is_an_extra_d_pad_and_the_arrows_stay() {
        for (key, arrow, button) in [
            (KeyCode::KeyW, KeyCode::ArrowUp, Button::Up),
            (KeyCode::KeyA, KeyCode::ArrowLeft, Button::Left),
            (KeyCode::KeyS, KeyCode::ArrowDown, Button::Down),
            (KeyCode::KeyD, KeyCode::ArrowRight, Button::Right),
        ] {
            assert_eq!(map(key), Some(Action::Button(button)), "{key:?}");
            assert_eq!(map(arrow), Some(Action::Button(button)), "{arrow:?}");
            let mut s = InputState::default();
            assert_eq!(s.key(key, true), Some((button, true)));
            assert_eq!(s.key(key, false), Some((button, false)));
        }
    }

    #[test]
    fn e_is_a_second_a_button_and_x_stays() {
        assert_eq!(map(KeyCode::KeyE), Some(Action::Button(Button::A)));
        assert_eq!(map(KeyCode::KeyX), Some(Action::Button(Button::A)));
        let mut s = InputState::default();
        assert_eq!(s.key(KeyCode::KeyE, true), Some((Button::A, true)));
        assert_eq!(s.key(KeyCode::KeyX, true), None, "A is already down");
        assert_eq!(s.key(KeyCode::KeyE, false), None, "X still holds it");
        assert_eq!(s.key(KeyCode::KeyX, false), Some((Button::A, false)));
    }

    #[test]
    fn save_state_moved_to_k_and_s_is_down() {
        assert_eq!(map(KeyCode::KeyK), Some(Action::SaveState));
        assert_eq!(map(KeyCode::KeyL), Some(Action::LoadState));
        assert_eq!(map(KeyCode::KeyS), Some(Action::Button(Button::Down)));
        let mut s = InputState::default();
        assert_eq!(s.key(KeyCode::KeyK, true), None, "K moves no button");
        // The startup notice names the same keys.
        assert!(SAVE_KEY_NOTICE.contains("K") && SAVE_KEY_NOTICE.contains("L"));
        assert!(SAVE_KEY_NOTICE.contains("S is now D-pad Down"));
    }

    #[test]
    fn q_and_r_are_the_gba_shoulders() {
        assert_eq!(map(KeyCode::KeyQ), Some(Action::Button(Button::L)));
        assert_eq!(map(KeyCode::KeyR), Some(Action::Button(Button::R)));
        let mut s = InputState::default();
        assert_eq!(s.key(KeyCode::KeyQ, true), Some((Button::L, true)));
        assert_eq!(s.key(KeyCode::KeyR, true), Some((Button::R, true)));
        assert_eq!(s.key(KeyCode::KeyQ, false), Some((Button::L, false)));
    }

    #[test]
    fn down_via_s_and_via_the_arrow_releases_only_when_both_are_up() {
        let mut s = InputState::default();
        assert_eq!(s.key(KeyCode::KeyS, true), Some((Button::Down, true)));
        assert_eq!(s.key(KeyCode::ArrowDown, true), None);
        assert_eq!(
            s.key(KeyCode::KeyS, false),
            None,
            "the arrow still holds it"
        );
        assert!(s.is_down(Button::Down));
        assert_eq!(
            s.key(KeyCode::ArrowDown, false),
            Some((Button::Down, false))
        );
        assert!(!s.is_down(Button::Down));
        // The other order, and focus loss with both held.
        assert_eq!(s.key(KeyCode::ArrowDown, true), Some((Button::Down, true)));
        assert_eq!(s.key(KeyCode::KeyS, true), None);
        assert_eq!(s.release_all(), vec![Button::Down]);
        assert_eq!(s.key(KeyCode::KeyS, false), None, "stale release");
    }

    #[test]
    fn shared_button_releases_only_when_both_keys_up() {
        let mut s = InputState::default();
        assert_eq!(
            s.key(KeyCode::Backspace, true),
            Some((Button::Select, true))
        );
        assert_eq!(s.key(KeyCode::ShiftRight, true), None);
        assert_eq!(s.key(KeyCode::Backspace, false), None);
        assert_eq!(
            s.key(KeyCode::ShiftRight, false),
            Some((Button::Select, false))
        );
    }

    #[test]
    fn non_button_keys_do_not_change_buttons() {
        let mut s = InputState::default();
        assert_eq!(s.key(KeyCode::Escape, true), None);
        assert_eq!(s.key(KeyCode::KeyJ, true), None);
    }

    #[test]
    fn release_all_returns_held_buttons() {
        let mut s = InputState::default();
        s.key(KeyCode::KeyX, true);
        s.key(KeyCode::ArrowLeft, true);
        let released = s.release_all();
        assert_eq!(released.len(), 2);
        assert!(released.contains(&Button::A) && released.contains(&Button::Left));
        assert!(s.release_all().is_empty());
        // A stale release after focus loss must not report a change.
        assert_eq!(s.key(KeyCode::KeyX, false), None);
    }
}
