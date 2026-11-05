//! `GameBoy` as an [`emu_common::Core`]: the boundary a frontend drives. Each
//! method forwards to the inherent one of the same name (an inherent method
//! always wins over a trait method), so the DMG/CGB hot path is untouched.
//! Debug hooks (`peek`, `cpu_registers`, serial, `step_instruction`) are not
//! part of the trait and stay on [`GameBoy`].

use emu_common::{BatteryError, Button, Core, FrameRef, Pixels, StateError, System};

use crate::{GameBoy, SCREEN_HEIGHT, SCREEN_WIDTH};

impl Core for GameBoy {
    fn system(&self) -> System {
        System::Gb
    }

    fn title(&self) -> String {
        self.cartridge_header().title.trim().to_owned()
    }

    fn native_fps(&self) -> f64 {
        crate::FRAMES_PER_SECOND
    }

    fn step_frame(&mut self) {
        GameBoy::step_frame(self);
    }

    /// The RGB555 frame of a CGB in colour mode, the shade frame otherwise.
    fn frame(&self) -> FrameRef<'_> {
        let pixels = match self.cgb_framebuffer() {
            Some(fb) => Pixels::Rgb555(&fb[..]),
            None => Pixels::Shades(&self.framebuffer()[..]),
        };
        FrameRef {
            width: SCREEN_WIDTH,
            height: SCREEN_HEIGHT,
            pixels,
        }
    }

    /// `L` and `R` do not exist on a Game Boy and are ignored.
    fn set_button(&mut self, button: Button, pressed: bool) {
        if let Some(button) = crate::Button::from_common(button) {
            GameBoy::set_button(self, button, pressed);
        }
    }

    fn drain_audio_into(&mut self, out: &mut Vec<f32>) {
        GameBoy::drain_audio_into(self, out);
    }

    fn audio_sample_rate(&self) -> u32 {
        GameBoy::audio_sample_rate(self)
    }

    fn set_audio_sample_rate(&mut self, hz: u32) {
        GameBoy::set_audio_sample_rate(self, hz);
    }

    fn battery_ram(&self) -> Option<Vec<u8>> {
        GameBoy::battery_ram(self)
    }

    fn battery_generation(&self) -> u64 {
        GameBoy::battery_generation(self)
    }

    fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), BatteryError> {
        GameBoy::load_battery_ram(self, data).map_err(Into::into)
    }

    /// `Model::id`: 0 DMG, 1 CGB.
    fn hw_variant(&self) -> u8 {
        self.model().id()
    }

    fn save_state(&self) -> Vec<u8> {
        GameBoy::save_state(self)
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), StateError> {
        GameBoy::load_state(self, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Model;

    fn rom(cgb: bool) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x134..0x13A].copy_from_slice(b" TITLE");
        rom[0x143] = if cgb { 0xC0 } else { 0 };
        rom[0x147] = 0x03; // MBC1+RAM+BATTERY
        rom[0x149] = 0x02;
        rom
    }

    #[test]
    fn a_game_boy_reports_itself_through_the_trait() {
        let mut gb = GameBoy::with_model(rom(false), Model::Dmg).unwrap();
        let core: &mut dyn Core = &mut gb;
        assert_eq!(core.system(), System::Gb);
        assert_eq!(core.title(), "TITLE");
        assert_eq!(core.hw_variant(), 0);
        assert!((core.native_fps() - 59.7275).abs() < 1e-3);
        core.step_frame();
        let f = core.frame();
        assert_eq!((f.width, f.height), (160, 144));
        assert!(matches!(f.pixels, Pixels::Shades(p) if p.len() == 160 * 144));
        assert_eq!(core.battery_ram().map(|r| r.len()), Some(0x2000));
        assert!(core.load_battery_ram(&[0; 3]).is_err());
    }

    #[test]
    fn a_cgb_in_colour_mode_hands_out_rgb555() {
        let mut gb = GameBoy::with_model(rom(true), Model::Cgb).unwrap();
        let core: &mut dyn Core = &mut gb;
        assert_eq!(core.hw_variant(), 1);
        core.step_frame();
        assert!(matches!(core.frame().pixels, Pixels::Rgb555(p) if p.len() == 160 * 144));
    }

    #[test]
    fn buttons_map_and_l_r_are_ignored() {
        let mut a = GameBoy::new(rom(false)).unwrap();
        let mut b = GameBoy::new(rom(false)).unwrap();
        for button in emu_common::Button::ALL {
            Core::set_button(&mut a, button, true);
        }
        for button in crate::Button::ALL {
            GameBoy::set_button(&mut b, button, true);
        }
        assert_eq!(a.save_state(), b.save_state(), "L and R changed nothing");
    }

    #[test]
    fn states_round_trip_through_the_trait() {
        let mut gb = GameBoy::new(rom(false)).unwrap();
        gb.step_frame();
        let snap = Core::save_state(&gb);
        gb.step_frame();
        Core::load_state(&mut gb, &snap).unwrap();
        assert_eq!(Core::save_state(&gb), snap);
        assert_eq!(Core::load_state(&mut gb, b"x"), Err(StateError::BadMagic));
    }
}
