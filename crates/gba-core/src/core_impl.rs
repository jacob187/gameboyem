//! `Gba` as an [`emu_common::Core`]: the boundary a frontend drives. Each method
//! forwards to the inherent one of the same name (an inherent method always wins
//! over a trait method). Debug hooks (`peek*`, `cpu_registers`,
//! `step_instruction`) are not part of the trait and stay on [`Gba`].

use emu_common::{BatteryError, Button, Core, FrameRef, Pixels, StateError, System};

use crate::cart::SaveKind;
use crate::{Gba, SCREEN_HEIGHT, SCREEN_WIDTH};

/// The GBA's own name for a button. `emu_common::Button` is in recording order
/// (the Game Boy's eight first, then L and R), the GBA's in KEYINPUT bit order.
fn to_gba(button: Button) -> crate::Button {
    match button {
        Button::A => crate::Button::A,
        Button::B => crate::Button::B,
        Button::Select => crate::Button::Select,
        Button::Start => crate::Button::Start,
        Button::Right => crate::Button::Right,
        Button::Left => crate::Button::Left,
        Button::Up => crate::Button::Up,
        Button::Down => crate::Button::Down,
        Button::L => crate::Button::L,
        Button::R => crate::Button::R,
    }
}

impl Core for Gba {
    fn system(&self) -> System {
        System::Gba
    }

    fn title(&self) -> String {
        Gba::title(self).trim().to_owned()
    }

    fn native_fps(&self) -> f64 {
        crate::FRAMES_PER_SECOND
    }

    fn step_frame(&mut self) {
        Gba::step_frame(self);
    }

    fn frame(&self) -> FrameRef<'_> {
        FrameRef {
            width: SCREEN_WIDTH,
            height: SCREEN_HEIGHT,
            pixels: Pixels::Rgb555(&self.framebuffer()[..]),
        }
    }

    fn set_button(&mut self, button: Button, pressed: bool) {
        Gba::set_button(self, to_gba(button), pressed);
    }

    fn drain_audio_into(&mut self, out: &mut Vec<f32>) {
        Gba::drain_audio_into(self, out);
    }

    fn audio_sample_rate(&self) -> u32 {
        Gba::audio_sample_rate(self)
    }

    fn set_audio_sample_rate(&mut self, hz: u32) {
        Gba::set_audio_sample_rate(self, hz);
    }

    /// `None` without a backup chip, and for an EEPROM until its size is known.
    /// Includes the RTC footer when the cartridge's clock is in use.
    fn battery_ram(&self) -> Option<Vec<u8>> {
        Gba::battery_save(self)
    }

    fn battery_generation(&self) -> u64 {
        Gba::battery_generation(self)
    }

    /// True for any backup chip, including an EEPROM whose size is not yet known.
    fn has_battery(&self) -> bool {
        self.save_kind() != SaveKind::None
    }

    fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), BatteryError> {
        Gba::load_battery_ram(self, data).map_err(Into::into)
    }

    /// A GBA has one hardware variant.
    fn hw_variant(&self) -> u8 {
        0
    }

    /// GBA save states come in P3 (DECISIONS #62).
    fn supports_save_state(&self) -> bool {
        false
    }

    /// Empty: there is no state format yet. A host checks
    /// [`Core::supports_save_state`] first.
    fn save_state(&self) -> Vec<u8> {
        Vec::new()
    }

    fn load_state(&mut self, _data: &[u8]) -> Result<(), StateError> {
        Err(StateError::Invalid("GBA save states are not supported yet"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYINPUT: u32 = 0x0400_0130;

    /// A 512-byte image: `b .` at the entry point, `title` at 0xA0, and a
    /// library ID string (`FLASH1M_V`, `EEPROM_V`...) at 0x100 if given.
    fn rom(title: &[u8], id: Option<&[u8]>) -> Vec<u8> {
        let mut rom = vec![0u8; 0x200];
        rom[..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
        rom[0xA0..0xA0 + title.len()].copy_from_slice(title);
        if let Some(id) = id {
            rom[0x100..0x100 + id.len()].copy_from_slice(id);
        }
        rom
    }

    fn gba(id: Option<&[u8]>) -> Gba {
        Gba::new(rom(b"TESTGAME", id), None).unwrap()
    }

    #[test]
    fn a_gba_reports_itself_through_the_trait() {
        let mut g = gba(None);
        let core: &mut dyn Core = &mut g;
        assert_eq!(core.system(), System::Gba);
        assert_eq!(core.title(), "TESTGAME");
        assert_eq!(core.hw_variant(), 0);
        assert_eq!(core.native_fps(), crate::FRAMES_PER_SECOND);
        core.step_frame();
        let f = core.frame();
        assert_eq!((f.width, f.height), (240, 160));
        assert!(matches!(f.pixels, Pixels::Rgb555(p) if p.len() == 240 * 160));
    }

    /// Every common button reaches its own KEYINPUT bit (active low), and L and
    /// R are the GBA's shoulders, bits 9 and 8.
    #[test]
    fn buttons_map_to_their_keyinput_bits() {
        let bit = |b: Button| match b {
            Button::A => 0,
            Button::B => 1,
            Button::Select => 2,
            Button::Start => 3,
            Button::Right => 4,
            Button::Left => 5,
            Button::Up => 6,
            Button::Down => 7,
            Button::R => 8,
            Button::L => 9,
        };
        let mut g = gba(None);
        assert_eq!(g.peek16(KEYINPUT) & 0x3FF, 0x3FF, "nothing pressed");
        for button in Button::ALL {
            Core::set_button(&mut g, button, true);
            assert_eq!(
                !g.peek16(KEYINPUT) & 0x3FF,
                1 << bit(button),
                "{button:?} alone"
            );
            Core::set_button(&mut g, button, false);
        }
        assert_eq!(g.peek16(KEYINPUT) & 0x3FF, 0x3FF, "all released");
    }

    #[test]
    fn save_states_are_not_supported_and_do_not_panic() {
        let mut g = gba(None);
        let core: &mut dyn Core = &mut g;
        assert!(!core.supports_save_state());
        assert!(core.save_state().is_empty());
        assert!(core.load_state(b"anything").is_err());
        // Boxed, as the frontend holds it: the default must not leak through.
        let boxed: Box<dyn Core> = Box::new(gba(None));
        assert!(!boxed.supports_save_state());
    }

    #[test]
    fn the_battery_follows_the_cartridge() {
        let none = gba(None);
        assert!(!Core::has_battery(&none) && Core::battery_ram(&none).is_none());

        let mut flash = gba(Some(b"FLASH1M_V"));
        assert!(Core::has_battery(&flash));
        assert_eq!(Core::battery_ram(&flash).map(|r| r.len()), Some(128 * 1024));
        assert!(Core::load_battery_ram(&mut flash, &[0; 3]).is_err());
        assert!(Core::load_battery_ram(&mut flash, &vec![0; 128 * 1024]).is_ok());

        // An EEPROM has a battery from power-on but no image until it is sized;
        // a 512-byte save sizes it.
        let mut eeprom = gba(Some(b"EEPROM_V"));
        assert!(Core::has_battery(&eeprom));
        assert_eq!(Core::battery_ram(&eeprom), None);
        assert!(Core::load_battery_ram(&mut eeprom, &[0; 100]).is_err());
        assert_eq!(
            Core::battery_ram(&eeprom),
            None,
            "a bad size changes nothing"
        );
        assert!(Core::load_battery_ram(&mut eeprom, &[0xAB; 512]).is_ok());
        assert_eq!(Core::battery_ram(&eeprom), Some(vec![0xAB; 512]));
    }

    #[test]
    fn audio_goes_through_the_facade() {
        let mut g = gba(None);
        let core: &mut dyn Core = &mut g;
        core.set_audio_sample_rate(44_100);
        assert_eq!(core.audio_sample_rate(), 44_100);
        core.step_frame();
        // The APU lands separately; whatever it produces is appended, as stereo.
        let mut out = vec![0.5];
        core.drain_audio_into(&mut out);
        assert_eq!(out[0], 0.5, "appends, never clears");
        assert_eq!(out.len() % 2, 1, "whole stereo frames after the marker");
    }
}
