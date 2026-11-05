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

pub fn map(key: KeyCode) -> Option<Action> {
    Some(match key {
        KeyCode::ArrowUp => Action::Button(Button::Up),
        KeyCode::ArrowDown => Action::Button(Button::Down),
        KeyCode::ArrowLeft => Action::Button(Button::Left),
        KeyCode::ArrowRight => Action::Button(Button::Right),
        KeyCode::KeyX => Action::Button(Button::A),
        KeyCode::KeyZ => Action::Button(Button::B),
        KeyCode::Enter | KeyCode::NumpadEnter => Action::Button(Button::Start),
        KeyCode::Backspace | KeyCode::ShiftRight => Action::Button(Button::Select),
        KeyCode::Escape => Action::Quit,
        KeyCode::KeyP => Action::Pause,
        KeyCode::Tab => Action::FastForward,
        KeyCode::KeyS => Action::SaveState,
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
        assert_eq!(map(KeyCode::KeyS), Some(Action::SaveState));
        assert_eq!(map(KeyCode::KeyL), Some(Action::LoadState));
        assert_eq!(map(KeyCode::Digit1), Some(Action::SelectSlot(1)));
        assert_eq!(map(KeyCode::Digit9), Some(Action::SelectSlot(9)));
        assert_eq!(map(KeyCode::Digit0), None);
        assert_eq!(map(KeyCode::KeyC), Some(Action::Screenshot));
        assert_eq!(map(KeyCode::KeyQ), None);
    }

    #[test]
    fn every_non_button_action_has_its_own_key() {
        // Screenshot (C) must not collide with save states (S, L, 1-9) or the
        // joypad: each of these keys maps to a different action.
        let keys = [
            KeyCode::KeyC,
            KeyCode::KeyS,
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
        for key in [KeyCode::KeyC, KeyCode::KeyS, KeyCode::KeyL, KeyCode::Digit1] {
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
        assert_eq!(s.key(KeyCode::KeyQ, true), None);
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
