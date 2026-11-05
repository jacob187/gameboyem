//! What a frontend needs from an emulator core, independent of the console:
//! [`Core`], the pictures it produces ([`FrameRef`]) and the buttons it takes
//! ([`Button`]).
//!
//! Console-specific debug hooks (CPU registers, memory peeks, serial output)
//! are not part of the trait; they stay on the concrete machine type.

use std::error::Error;

use crate::state::StateError;

/// Which console a core emulates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum System {
    /// Game Boy and Game Boy Color.
    Gb,
    /// Game Boy Advance.
    Gba,
}

impl System {
    /// The byte a recording stores for this system.
    pub const fn id(self) -> u8 {
        match self {
            Self::Gb => 0,
            Self::Gba => 1,
        }
    }

    /// The inverse of [`System::id`]; `None` for a byte no system writes.
    pub const fn from_id(id: u8) -> Option<Self> {
        match id {
            0 => Some(Self::Gb),
            1 => Some(Self::Gba),
            _ => None,
        }
    }

    /// How many of [`Button::ALL`] (a prefix of it) the console has: the
    /// bound a recording of this system is decoded with.
    pub const fn button_count(self) -> usize {
        match self {
            Self::Gb => 8,
            Self::Gba => 10,
        }
    }

    /// Whether the console has `button`.
    pub const fn has_button(self, button: Button) -> bool {
        (button.index() as usize) < self.button_count()
    }
}

/// A joypad button of any supported console.
///
/// The order is part of the recording format: a button is stored as its
/// position in [`Button::ALL`], and the first eight are the Game Boy's, in the
/// order `gb_core::Button::ALL` lists them. `L` and `R` exist only on the GBA.
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
    L,
    R,
}

impl Button {
    pub const ALL: [Button; 10] = [
        Button::Right,
        Button::Left,
        Button::Up,
        Button::Down,
        Button::A,
        Button::B,
        Button::Select,
        Button::Start,
        Button::L,
        Button::R,
    ];

    /// Position in [`Button::ALL`] (the byte a recording stores).
    pub const fn index(self) -> u8 {
        self as u8
    }

    /// The inverse of [`Button::index`].
    pub const fn from_index(index: u8) -> Option<Self> {
        if (index as usize) < Self::ALL.len() {
            Some(Self::ALL[index as usize])
        } else {
            None
        }
    }
}

/// The pixels of one frame, row-major, `width * height` of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pixels<'a> {
    /// Game Boy shades 0 (lightest) ..= 3 (darkest), after palette mapping.
    /// The host picks the colours.
    Shades(&'a [u8]),
    /// RGB555 as `0bbbbbgggggrrrrr` (red in the low 5 bits; bit 15 ignored):
    /// the Game Boy Color's and the Game Boy Advance's palette layout.
    Rgb555(&'a [u16]),
}

impl Pixels<'_> {
    pub fn len(&self) -> usize {
        match self {
            Self::Shades(p) => p.len(),
            Self::Rgb555(p) => p.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The last completed frame, borrowed from the core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameRef<'a> {
    pub width: usize,
    pub height: usize,
    pub pixels: Pixels<'a>,
}

/// Why a battery image was refused.
pub type BatteryError = Box<dyn Error + Send + Sync>;

/// An emulated console, as a frontend drives it. Object safe: the frontend
/// holds a `Box<dyn Core>` so the system can be chosen at run time.
///
/// A core is deterministic and does no I/O; everything here is pure state
/// access. The host supplies time by calling [`Core::step_frame`] at
/// [`Core::native_fps`].
pub trait Core {
    fn system(&self) -> System;

    /// The game's title from its header, trimmed; empty if it has none.
    fn title(&self) -> String;

    /// Frames per second of the real console.
    fn native_fps(&self) -> f64;

    /// Run until the next frame is complete (a core may return early or late
    /// by a fraction of a frame; callers pace by the audio they drain).
    fn step_frame(&mut self);

    /// The last completed frame.
    fn frame(&self) -> FrameRef<'_>;

    /// Press or release a button. A button the console lacks is ignored.
    fn set_button(&mut self, button: Button, pressed: bool);

    /// Append the buffered audio (interleaved stereo `f32`, -1.0..=1.0, at
    /// [`Core::audio_sample_rate`]) to `out` and clear the buffer.
    fn drain_audio_into(&mut self, out: &mut Vec<f32>);

    fn audio_sample_rate(&self) -> u32;

    /// Ask for an output rate; the core may clamp it, so read it back.
    fn set_audio_sample_rate(&mut self, hz: u32);

    /// The image to persist as the `.sav` file; `None` without a battery.
    /// Owned because a core may assemble it (a Game Boy RTC footer).
    fn battery_ram(&self) -> Option<Vec<u8>>;

    /// Advances only when the game changes battery-backed state (or a save is
    /// loaded), so a host can skip cloning [`Core::battery_ram`] while it is
    /// unchanged. Only meaningful when `battery_ram()` is `Some`.
    fn battery_generation(&self) -> u64;

    /// Whether the cartridge has battery-backed storage at all, even while
    /// [`Core::battery_ram`] is `None` (a GBA EEPROM's size, and so its image,
    /// is unknown until the game first uses it). A host loads an existing
    /// `.sav` only when this is true. Defaults to `battery_ram().is_some()`.
    fn has_battery(&self) -> bool {
        self.battery_ram().is_some()
    }

    /// Restore a `.sav` image.
    fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), BatteryError>;

    /// The system-specific hardware variant as a recording stores it (Game
    /// Boy: 0 DMG, 1 CGB); 0 where a system has only one.
    fn hw_variant(&self) -> u8;

    /// Whether [`Core::save_state`] and [`Core::load_state`] work. A core
    /// that returns false has no state format yet (the GBA until P3); a host
    /// must not call them, and says so instead. Defaults to true.
    fn supports_save_state(&self) -> bool {
        true
    }

    /// Snapshot the whole machine.
    fn save_state(&self) -> Vec<u8>;

    /// Restore a snapshot. All-or-nothing: on error the machine is unchanged.
    fn load_state(&mut self, data: &[u8]) -> Result<(), StateError>;
}

/// A boxed core is a core, so generic host code can hold either.
impl<T: Core + ?Sized> Core for Box<T> {
    fn system(&self) -> System {
        (**self).system()
    }
    fn title(&self) -> String {
        (**self).title()
    }
    fn native_fps(&self) -> f64 {
        (**self).native_fps()
    }
    fn step_frame(&mut self) {
        (**self).step_frame();
    }
    fn frame(&self) -> FrameRef<'_> {
        (**self).frame()
    }
    fn set_button(&mut self, button: Button, pressed: bool) {
        (**self).set_button(button, pressed);
    }
    fn drain_audio_into(&mut self, out: &mut Vec<f32>) {
        (**self).drain_audio_into(out);
    }
    fn audio_sample_rate(&self) -> u32 {
        (**self).audio_sample_rate()
    }
    fn set_audio_sample_rate(&mut self, hz: u32) {
        (**self).set_audio_sample_rate(hz);
    }
    fn battery_ram(&self) -> Option<Vec<u8>> {
        (**self).battery_ram()
    }
    fn battery_generation(&self) -> u64 {
        (**self).battery_generation()
    }
    fn has_battery(&self) -> bool {
        (**self).has_battery()
    }
    fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), BatteryError> {
        (**self).load_battery_ram(data)
    }
    fn hw_variant(&self) -> u8 {
        (**self).hw_variant()
    }
    fn supports_save_state(&self) -> bool {
        (**self).supports_save_state()
    }
    fn save_state(&self) -> Vec<u8> {
        (**self).save_state()
    }
    fn load_state(&mut self, data: &[u8]) -> Result<(), StateError> {
        (**self).load_state(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A core that records what it is asked, to check the trait is object
    /// safe and that `Box<dyn Core>` forwards.
    #[derive(Default)]
    struct Mock {
        pressed: Vec<(Button, bool)>,
        frames: u32,
    }

    impl Core for Mock {
        fn system(&self) -> System {
            System::Gba
        }
        fn title(&self) -> String {
            "mock".into()
        }
        fn native_fps(&self) -> f64 {
            60.0
        }
        fn step_frame(&mut self) {
            self.frames += 1;
        }
        fn frame(&self) -> FrameRef<'_> {
            FrameRef {
                width: 0,
                height: 0,
                pixels: Pixels::Shades(&[]),
            }
        }
        fn set_button(&mut self, button: Button, pressed: bool) {
            self.pressed.push((button, pressed));
        }
        fn drain_audio_into(&mut self, _out: &mut Vec<f32>) {}
        fn audio_sample_rate(&self) -> u32 {
            48_000
        }
        fn set_audio_sample_rate(&mut self, _hz: u32) {}
        fn battery_ram(&self) -> Option<Vec<u8>> {
            None
        }
        fn battery_generation(&self) -> u64 {
            0
        }
        fn load_battery_ram(&mut self, _data: &[u8]) -> Result<(), BatteryError> {
            Err("no battery".into())
        }
        fn hw_variant(&self) -> u8 {
            0
        }
        fn has_battery(&self) -> bool {
            true // while battery_ram() is None: a GBA EEPROM of unknown size
        }
        fn supports_save_state(&self) -> bool {
            false
        }
        fn save_state(&self) -> Vec<u8> {
            Vec::new()
        }
        fn load_state(&mut self, _data: &[u8]) -> Result<(), StateError> {
            Err(StateError::BadMagic)
        }
    }

    #[test]
    fn the_trait_is_object_safe_and_a_box_forwards() {
        let mut boxed: Box<dyn Core> = Box::new(Mock::default());
        boxed.step_frame();
        boxed.set_button(Button::L, true);
        assert_eq!(boxed.system(), System::Gba);
        assert_eq!(boxed.title(), "mock");
        assert!(boxed.load_battery_ram(&[]).is_err());
        // The defaulted methods must be forwarded, not answered by the default.
        assert!(boxed.has_battery() && boxed.battery_ram().is_none());
        assert!(!boxed.supports_save_state());
        // A `Box<dyn Core>` is itself a `Core`, for generic host code.
        fn drive(mut core: impl Core) -> System {
            core.step_frame();
            core.system()
        }
        assert_eq!(drive(boxed), System::Gba);
    }

    #[test]
    fn button_indices_round_trip_and_match_the_game_boy_order() {
        for (i, b) in Button::ALL.into_iter().enumerate() {
            assert_eq!(usize::from(b.index()), i);
            assert_eq!(Button::from_index(b.index()), Some(b));
        }
        assert_eq!(Button::from_index(10), None);
        assert_eq!(Button::from_index(255), None);
    }

    #[test]
    fn systems_have_the_buttons_they_have() {
        assert_eq!(System::from_id(0), Some(System::Gb));
        assert_eq!(System::from_id(1), Some(System::Gba));
        assert_eq!(System::from_id(2), None);
        for b in Button::ALL {
            assert!(System::Gba.has_button(b));
            assert_eq!(
                System::Gb.has_button(b),
                !matches!(b, Button::L | Button::R)
            );
        }
    }
}
