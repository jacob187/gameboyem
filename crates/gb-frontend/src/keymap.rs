//! Keyboard -> emulator actions. Keys are matched by physical position
//! (`KeyCode`), so Z/X stay put on non-QWERTY layouts.

use std::collections::HashSet;

use emu_common::Button;
use winit::keyboard::{KeyCode, PhysicalKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Button(Button),
    /// Esc: what it does depends on where the app is (see `ui::escape_effect`).
    Menu,
    Pause,
    FastForward,
    /// K: add a snapshot of the game.
    SaveSnapshot,
    /// L: go back to the most recent snapshot.
    LoadLatestSnapshot,
    /// Save the current picture as a PNG (and mark the frame in a recording).
    Screenshot,
}

impl Action {
    /// What the Help screen calls this action. Actions with one label share a row there
    /// (the keys that do the same).
    pub fn label(self) -> &'static str {
        match self {
            Action::Button(Button::Up) => "Up",
            Action::Button(Button::Down) => "Down",
            Action::Button(Button::Left) => "Left",
            Action::Button(Button::Right) => "Right",
            Action::Button(Button::A) => "A",
            Action::Button(Button::B) => "B",
            Action::Button(Button::Start) => "Start",
            Action::Button(Button::Select) => "Select",
            Action::Button(Button::L) => "L (GBA)",
            Action::Button(Button::R) => "R (GBA)",
            Action::Menu => "Pause menu / back",
            Action::Pause => "Pause",
            Action::FastForward => "Fast-forward (hold)",
            Action::SaveSnapshot => "Save snapshot",
            Action::LoadLatestSnapshot => "Load latest snapshot",
            Action::Screenshot => "Screenshot",
        }
    }
}

/// One key and what it does. `label` is how the Help screen names the key.
#[derive(Clone, Copy, Debug)]
pub struct Binding {
    pub key: KeyCode,
    pub label: &'static str,
    pub action: Action,
}

const fn bind(key: KeyCode, label: &'static str, action: Action) -> Binding {
    Binding { key, label, action }
}

/// Every key the app reacts to while a game is running. [`map`] looks keys up here and
/// the Help screen is built from it ([`help_rows`]), so the two cannot disagree.
pub const BINDINGS: &[Binding] = &[
    bind(KeyCode::ArrowUp, "Up arrow", Action::Button(Button::Up)),
    bind(KeyCode::KeyW, "W", Action::Button(Button::Up)),
    bind(
        KeyCode::ArrowDown,
        "Down arrow",
        Action::Button(Button::Down),
    ),
    bind(KeyCode::KeyS, "S", Action::Button(Button::Down)),
    bind(
        KeyCode::ArrowLeft,
        "Left arrow",
        Action::Button(Button::Left),
    ),
    bind(KeyCode::KeyA, "A", Action::Button(Button::Left)),
    bind(
        KeyCode::ArrowRight,
        "Right arrow",
        Action::Button(Button::Right),
    ),
    bind(KeyCode::KeyD, "D", Action::Button(Button::Right)),
    bind(KeyCode::KeyX, "X", Action::Button(Button::A)),
    bind(KeyCode::KeyE, "E", Action::Button(Button::A)),
    bind(KeyCode::KeyZ, "Z", Action::Button(Button::B)),
    bind(KeyCode::Enter, "Enter", Action::Button(Button::Start)),
    bind(
        KeyCode::NumpadEnter,
        "Numpad Enter",
        Action::Button(Button::Start),
    ),
    bind(
        KeyCode::Backspace,
        "Backspace",
        Action::Button(Button::Select),
    ),
    bind(
        KeyCode::ShiftRight,
        "Right Shift",
        Action::Button(Button::Select),
    ),
    // GBA shoulders; a Game Boy has neither and the app drops them.
    bind(KeyCode::KeyQ, "Q", Action::Button(Button::L)),
    bind(KeyCode::KeyR, "R", Action::Button(Button::R)),
    bind(KeyCode::Escape, "Esc", Action::Menu),
    bind(KeyCode::KeyP, "P", Action::Pause),
    bind(KeyCode::Tab, "Tab", Action::FastForward),
    bind(KeyCode::KeyK, "K", Action::SaveSnapshot),
    bind(KeyCode::KeyL, "L", Action::LoadLatestSnapshot),
    bind(KeyCode::KeyC, "C", Action::Screenshot),
];

/// Printed once when the window opens on a Game Boy, where S used to save a state.
pub const SAVE_KEY_NOTICE: &str =
    "snapshot is K, load the latest snapshot is L (S is now D-pad Down)";

pub fn map(key: KeyCode) -> Option<Action> {
    BINDINGS.iter().find(|b| b.key == key).map(|b| b.action)
}

/// Whether a key event goes to the menu in front (egui): every key over a menu but Esc,
/// which is the app's. egui takes each repeat of Enter or Space as another press of the
/// button that has the keyboard focus, so holding Enter on Save snapshot or a snapshot's
/// Load would act again with every repeat; those repeats are kept from it while a button
/// has the focus (`focus_on_button`). Anything else, a text field included, gets them.
pub fn menu_takes_key(
    menu_visible: bool,
    key: PhysicalKey,
    repeat: bool,
    focus_on_button: bool,
) -> bool {
    let clicks = matches!(
        key,
        PhysicalKey::Code(KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Space)
    );
    menu_visible
        && key != PhysicalKey::Code(KeyCode::Escape)
        && !(repeat && clicks && focus_on_button)
}

/// One line of the Help screen's controls table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelpRow {
    pub what: &'static str,
    pub keys: Vec<&'static str>,
}

/// The controls table: [`BINDINGS`] grouped by [`Action::label`], in table order.
pub fn help_rows() -> Vec<HelpRow> {
    let mut rows: Vec<HelpRow> = Vec::new();
    for b in BINDINGS {
        let what = b.action.label();
        match rows.iter_mut().find(|r| r.what == what) {
            Some(row) => row.keys.push(b.label),
            None => rows.push(HelpRow {
                what,
                keys: vec![b.label],
            }),
        }
    }
    rows
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
        assert_eq!(map(KeyCode::Escape), Some(Action::Menu));
        assert_eq!(map(KeyCode::KeyP), Some(Action::Pause));
        assert_eq!(map(KeyCode::Tab), Some(Action::FastForward));
        assert_eq!(map(KeyCode::KeyK), Some(Action::SaveSnapshot));
        assert_eq!(map(KeyCode::KeyL), Some(Action::LoadLatestSnapshot));
        assert_eq!(map(KeyCode::KeyC), Some(Action::Screenshot));
        assert_eq!(map(KeyCode::KeyJ), None);
    }

    #[test]
    fn every_non_button_action_has_its_own_key() {
        // Screenshot (C) must not collide with snapshots (K, L) or the joypad: each of
        // these keys maps to a different action.
        let keys = [
            KeyCode::KeyC,
            KeyCode::KeyK,
            KeyCode::KeyL,
            KeyCode::KeyP,
            KeyCode::Tab,
            KeyCode::Escape,
        ];
        let mut seen = Vec::new();
        for key in keys {
            let action = map(key).expect("bound");
            assert!(!seen.contains(&action), "{action:?} is bound twice");
            seen.push(action);
        }
        assert_eq!(map(KeyCode::KeyC), Some(Action::Screenshot));
        let mut s = InputState::default();
        for key in [KeyCode::KeyC, KeyCode::KeyK, KeyCode::KeyL] {
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
    fn snapshots_are_k_and_l_and_s_is_down() {
        assert_eq!(map(KeyCode::KeyK), Some(Action::SaveSnapshot));
        assert_eq!(map(KeyCode::KeyL), Some(Action::LoadLatestSnapshot));
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

    #[test]
    fn a_held_enter_or_space_does_not_click_a_focused_button_again() {
        let code = PhysicalKey::Code;
        for key in [KeyCode::Enter, KeyCode::NumpadEnter, KeyCode::Space] {
            assert!(menu_takes_key(true, code(key), false, true), "{key:?} once");
            assert!(
                !menu_takes_key(true, code(key), true, true),
                "{key:?} repeat"
            );
            // With no button in focus (a text field, nothing) the repeats go through.
            assert!(menu_takes_key(true, code(key), true, false), "{key:?}");
        }
        // Arrows and Tab still repeat (moving the focus); Esc is never the menu's.
        assert!(menu_takes_key(true, code(KeyCode::ArrowDown), true, true));
        assert!(menu_takes_key(true, code(KeyCode::Tab), true, true));
        assert!(!menu_takes_key(true, code(KeyCode::Escape), false, false));
        // While playing no key reaches the menu.
        assert!(!menu_takes_key(false, code(KeyCode::Enter), false, false));
        assert!(!menu_takes_key(false, code(KeyCode::KeyK), false, false));
    }

    #[test]
    fn map_reads_the_binding_table() {
        for b in BINDINGS {
            assert_eq!(map(b.key), Some(b.action), "{}", b.label);
        }
        assert_eq!(map(KeyCode::KeyJ), None);
    }

    #[test]
    fn no_key_is_bound_or_labelled_twice() {
        for (i, a) in BINDINGS.iter().enumerate() {
            for b in &BINDINGS[i + 1..] {
                assert_ne!(a.key, b.key, "{} is bound twice", a.label);
                assert_ne!(a.label, b.label, "two keys are called {}", a.label);
            }
        }
    }

    #[test]
    fn help_rows_list_exactly_the_bindings() {
        let rows = help_rows();
        // Every binding appears under its action's row, and nothing else does.
        let mut listed = 0;
        for b in BINDINGS {
            let row = rows
                .iter()
                .find(|r| r.what == b.action.label())
                .unwrap_or_else(|| panic!("no help row for {}", b.label));
            assert!(
                row.keys.contains(&b.label),
                "{} is missing from its row",
                b.label
            );
            listed += 1;
        }
        assert_eq!(rows.iter().map(|r| r.keys.len()).sum::<usize>(), listed);
        assert!(rows.iter().all(|r| !r.keys.is_empty()));
        let mut names: Vec<_> = rows.iter().map(|r| r.what).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), rows.len(), "one row per label");
    }

    #[test]
    fn help_groups_the_keys_the_player_knows() {
        let rows = help_rows();
        let keys = |what: &str| {
            rows.iter()
                .find(|r| r.what == what)
                .map(|r| r.keys.clone())
                .unwrap()
        };
        assert_eq!(keys("A"), ["X", "E"]);
        assert_eq!(keys("Select"), ["Backspace", "Right Shift"]);
        assert_eq!(keys("Pause menu / back"), ["Esc"]);
        assert_eq!(keys("Save snapshot"), ["K"]);
        assert_eq!(keys("Load latest snapshot"), ["L"]);
    }

    #[test]
    fn there_are_no_slot_keys() {
        let digits = [
            KeyCode::Digit0,
            KeyCode::Digit1,
            KeyCode::Digit2,
            KeyCode::Digit3,
            KeyCode::Digit4,
            KeyCode::Digit5,
            KeyCode::Digit6,
            KeyCode::Digit7,
            KeyCode::Digit8,
            KeyCode::Digit9,
            KeyCode::Numpad1,
        ];
        for key in digits {
            assert_eq!(map(key), None, "{key:?}");
        }
        assert!(
            BINDINGS
                .iter()
                .all(|b| !b.label.chars().all(|c| c.is_ascii_digit()))
        );
        assert!(
            help_rows()
                .iter()
                .all(|r| !r.what.to_lowercase().contains("slot"))
        );
    }
}
