//! Save-state slots: `<rom>.ss1` ... `<rom>.ss9` next to the ROM, written
//! atomically. Loading a state also rewinds battery RAM, which the battery
//! saver then writes to `<rom>.sav`, so the first load of a session first
//! backs that file up.

use std::path::{Path, PathBuf};

use emu_common::{Core, StateError, System};
use gb_core::Model;

use crate::save::{append_ext, backup_without_overwrite, read_capped, save_path, write_atomic};

pub const FIRST_SLOT: u8 = 1;
pub const LAST_SLOT: u8 = 9;

/// Larger than any Game Boy state the core produces. The fixed machine state is
/// about 62 KiB on a DMG and 139 KiB in CGB colour mode (both VRAM and WRAM banks
/// plus the RGB555 back and front frames), plus at most 128 KiB of cart RAM: 267 KiB
/// at worst, which is why this is 512 KiB and not 256 KiB. A test checks the
/// largest cases.
const GB_MAX_STATE_BYTES: u64 = 512 * 1024;

/// The largest save state a core of `system` may produce; a bigger file is
/// refused unread, so a bogus file cannot exhaust memory.
pub fn max_state_bytes(system: System) -> u64 {
    match system {
        System::Gb => GB_MAX_STATE_BYTES,
        // From the core: the real worst case (128 KiB flash) plus a little.
        System::Gba => gba_core::MAX_STATE_BYTES as u64,
    }
}

/// `game.gb` -> `game.ss<slot>`. Like the battery save, the extension is
/// appended instead if that would name the ROM itself.
pub fn state_path(rom: &Path, slot: u8) -> PathBuf {
    let ext = format!("ss{slot}");
    let path = rom.with_extension(&ext);
    if path == rom {
        append_ext(rom, &ext)
    } else {
        path
    }
}

/// The text for a failed `load_state`. A GBA mismatch names the options a state
/// is tied to. A Game Boy model mismatch names the way out: a pre-CGB (v1) state is a DMG one, and `--model auto` now starts
/// CGB-flagged ROMs as a Game Boy Color, so they need `--model dmg` to load it.
/// `variant` is [`Core::hw_variant`] (Game Boy: 0 DMG, 1 CGB), decoded through
/// [`Model::from_id`] so that a new model fails to compile here until it has a hint.
fn load_error(e: &StateError, system: System, variant: u8) -> String {
    match (e, system) {
        (StateError::ModelMismatch, System::Gb) => match Model::from_id(variant) {
            Some(Model::Cgb) => format!(
                "{e}; this session is a Game Boy Color, for a DMG state start with --model dmg"
            ),
            Some(Model::Dmg) => format!(
                "{e}; this session is a DMG, for a Game Boy Color state start with --model cgb"
            ),
            None => e.to_string(),
        },
        // A GBA state is tied to the BIOS and the cartridge options it ran with.
        (StateError::ModelMismatch, System::Gba) => {
            format!("{e}; start with the same --bios, --save-type and --rtc as when it was saved")
        }
        _ => e.to_string(),
    }
}

/// What K and L report for a core that has no state format ([`Core::supports_save_state`]).
const NO_STATES: &str = "this core has no save states";

pub struct StateSlots {
    rom: PathBuf,
    slot: u8,
    /// The `.sav` has been backed up (or there was none) this session.
    sav_backed_up: bool,
}

impl StateSlots {
    pub fn new(rom: &Path) -> Self {
        Self {
            rom: rom.to_owned(),
            slot: FIRST_SLOT,
            sav_backed_up: false,
        }
    }

    pub fn slot(&self) -> u8 {
        self.slot
    }

    pub fn select(&mut self, slot: u8) {
        if (FIRST_SLOT..=LAST_SLOT).contains(&slot) {
            self.slot = slot;
        }
    }

    pub fn path(&self) -> PathBuf {
        state_path(&self.rom, self.slot)
    }

    /// Write the current slot. Returns a message for the user.
    pub fn save(&self, core: &dyn Core) -> Result<String, String> {
        if !core.supports_save_state() {
            return Err(NO_STATES.to_owned());
        }
        let path = self.path();
        write_atomic(&path, &core.save_state())
            .map(|()| format!("saved state {}", self.slot))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// Load the current slot into `core`. On error `core` is unchanged.
    pub fn load(&mut self, core: &mut dyn Core) -> Result<String, String> {
        if !core.supports_save_state() {
            return Err(NO_STATES.to_owned());
        }
        let path = self.path();
        let data = match read_capped(&path, max_state_bytes(core.system()), "save state") {
            Ok(Some(d)) => d,
            Ok(None) => return Err(format!("state {} is empty", self.slot)),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let backup = self.backup_sav_once()?;
        core.load_state(&data).map_err(|e| {
            format!(
                "cannot load state {}: {}",
                self.slot,
                load_error(&e, core.system(), core.hw_variant())
            )
        })?;
        self.sav_backed_up = true;
        Ok(match backup {
            Some(b) => format!(
                "loaded state {} (previous save kept as {})",
                self.slot,
                b.display()
            ),
            None => format!("loaded state {}", self.slot),
        })
    }

    /// Before the first load of a session, copy `<rom>.sav` aside: the loaded
    /// state's battery RAM replaces it on the next autosave. Refuses the load
    /// if the copy fails, so real progress is never silently overwritten.
    fn backup_sav_once(&self) -> Result<Option<PathBuf>, String> {
        let sav = save_path(&self.rom);
        if self.sav_backed_up || !sav.is_file() {
            return Ok(None);
        }
        backup_without_overwrite(&sav)
            .map(Some)
            .map_err(|e| format!("not loading: cannot back up {} first: {e}", sav.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gb_core::{GameBoy, Model};
    use std::fs;

    fn battery_rom(ram_code: u8) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x147] = 0x1B; // MBC5+RAM+BATTERY
        rom[0x149] = ram_code;
        rom
    }

    /// A Game Boy that says it cannot save states.
    struct NoStates(GameBoy);

    impl Core for NoStates {
        fn system(&self) -> System {
            Core::system(&self.0)
        }
        fn title(&self) -> String {
            Core::title(&self.0)
        }
        fn native_fps(&self) -> f64 {
            Core::native_fps(&self.0)
        }
        fn step_frame(&mut self) {
            Core::step_frame(&mut self.0);
        }
        fn frame(&self) -> emu_common::FrameRef<'_> {
            Core::frame(&self.0)
        }
        fn set_button(&mut self, button: emu_common::Button, pressed: bool) {
            Core::set_button(&mut self.0, button, pressed);
        }
        fn drain_audio_into(&mut self, out: &mut Vec<f32>) {
            Core::drain_audio_into(&mut self.0, out);
        }
        fn audio_sample_rate(&self) -> u32 {
            Core::audio_sample_rate(&self.0)
        }
        fn set_audio_sample_rate(&mut self, hz: u32) {
            Core::set_audio_sample_rate(&mut self.0, hz);
        }
        fn battery_ram(&self) -> Option<Vec<u8>> {
            Core::battery_ram(&self.0)
        }
        fn battery_generation(&self) -> u64 {
            Core::battery_generation(&self.0)
        }
        fn load_battery_ram(&mut self, data: &[u8]) -> Result<(), emu_common::BatteryError> {
            Core::load_battery_ram(&mut self.0, data)
        }
        fn hw_variant(&self) -> u8 {
            Core::hw_variant(&self.0)
        }
        fn supports_save_state(&self) -> bool {
            false
        }
        fn save_state(&self) -> Vec<u8> {
            unreachable!("the host must not call save_state")
        }
        fn load_state(&mut self, _: &[u8]) -> Result<(), StateError> {
            unreachable!("the host must not call load_state")
        }
    }

    #[test]
    fn a_core_without_states_is_not_asked_for_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut slots = StateSlots::new(&dir.path().join("g.gb"));
        let mut core = NoStates(GameBoy::new(battery_rom(0x02)).unwrap());
        assert_eq!(slots.save(&core), Err(NO_STATES.to_owned()));
        assert_eq!(slots.load(&mut core), Err(NO_STATES.to_owned()));
        assert!(!slots.path().exists(), "nothing was written");
    }

    #[test]
    fn slot_paths() {
        assert_eq!(state_path(Path::new("d/g.gb"), 3), PathBuf::from("d/g.ss3"));
        assert_eq!(
            state_path(Path::new("d/g.ss3"), 3),
            PathBuf::from("d/g.ss3.ss3")
        );
        let mut s = StateSlots::new(Path::new("g.gb"));
        assert_eq!(s.slot(), 1);
        s.select(9);
        assert_eq!(s.slot(), 9);
        s.select(0);
        s.select(10);
        assert_eq!(s.slot(), 9, "out-of-range slots are ignored");
    }

    #[test]
    fn the_gba_cap_comes_from_the_core() {
        assert_eq!(
            max_state_bytes(System::Gba),
            gba_core::MAX_STATE_BYTES as u64
        );
        assert_eq!(max_state_bytes(System::Gb), GB_MAX_STATE_BYTES);
    }

    #[test]
    fn largest_state_fits_the_cap() {
        let gb = GameBoy::new(battery_rom(0x04)).unwrap(); // 128 KiB RAM
        let len = gb.save_state().len() as u64;
        assert!(len <= GB_MAX_STATE_BYTES, "{len}");
    }

    /// A CGB-only cartridge (header flag 0xC0) so the machine runs in colour mode.
    fn cgb_rom(ram_code: u8) -> Vec<u8> {
        let mut rom = battery_rom(ram_code);
        rom[0x143] = 0xC0;
        rom
    }

    #[test]
    fn cgb_states_fit_the_cap_with_margin() {
        // Measured: 142 011 bytes with no cart RAM, 273 083 with 128 KiB (the
        // MBC5 maximum). Both must stay under the cap, and the largest keeps
        // at least ~100 KiB of room for future state.
        let mut largest = 0;
        for (ram_code, ram_len) in [(0x00, 0), (0x04, 128 * 1024)] {
            let gb = GameBoy::with_model(cgb_rom(ram_code), Model::Cgb).unwrap();
            assert!(gb.cgb_framebuffer().is_some(), "colour mode");
            let len = gb.save_state().len() as u64;
            assert!(len < GB_MAX_STATE_BYTES, "{len}");
            assert!(len < 150_000 + ram_len, "{len}: the fixed part grew");
            largest = len;
        }
        assert!(
            GB_MAX_STATE_BYTES - largest >= 100 * 1024,
            "margin {}",
            GB_MAX_STATE_BYTES - largest
        );
    }

    #[test]
    fn a_model_mismatch_names_the_model_flag() {
        let dir = tempfile::tempdir().unwrap();
        let rom = dir.path().join("g.gb");
        let mut slots = StateSlots::new(&rom);
        let dmg = GameBoy::with_model(cgb_rom(0x02), Model::Dmg).unwrap();
        let mut cgb = GameBoy::with_model(cgb_rom(0x02), Model::Cgb).unwrap();
        slots.save(&dmg).unwrap(); // a DMG state, as every pre-CGB state is
        let err = slots.load(&mut cgb).unwrap_err();
        assert!(err.contains("different hardware model"), "{err}");
        assert!(err.contains("--model dmg"), "{err}");

        slots.save(&cgb).unwrap();
        let mut dmg = dmg;
        assert!(slots.load(&mut dmg).unwrap_err().contains("--model cgb"));
    }

    #[test]
    fn save_then_load_restores_the_machine() {
        let dir = tempfile::tempdir().unwrap();
        let rom = dir.path().join("g.gb");
        let mut gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let mut slots = StateSlots::new(&rom);
        slots.select(2);
        gb.step_frame();
        let snap = gb.save_state();
        slots.save(&gb).unwrap();
        assert!(dir.path().join("g.ss2").is_file());

        for _ in 0..10 {
            gb.step_frame();
        }
        assert_ne!(gb.save_state(), snap);
        slots.load(&mut gb).unwrap();
        assert_eq!(gb.save_state(), snap);
    }

    #[test]
    fn failed_loads_leave_the_machine_alone() {
        let dir = tempfile::tempdir().unwrap();
        let rom = dir.path().join("g.gb");
        let mut gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let before = gb.save_state();
        let mut slots = StateSlots::new(&rom);
        assert!(slots.load(&mut gb).unwrap_err().contains("empty"));
        fs::write(dir.path().join("g.ss1"), b"not a state").unwrap();
        assert!(
            slots
                .load(&mut gb)
                .unwrap_err()
                .contains("not a save state")
        );
        let other = GameBoy::new(battery_rom(0x03)).unwrap().save_state();
        fs::write(dir.path().join("g.ss1"), other).unwrap();
        assert!(slots.load(&mut gb).unwrap_err().contains("different ROM"));
        fs::write(
            dir.path().join("g.ss1"),
            vec![0; GB_MAX_STATE_BYTES as usize + 1],
        )
        .unwrap();
        assert!(slots.load(&mut gb).unwrap_err().contains("larger than"));
        assert_eq!(gb.save_state(), before);
    }

    #[test]
    fn first_load_backs_up_the_battery_save_once() {
        let dir = tempfile::tempdir().unwrap();
        let rom = dir.path().join("g.gb");
        let sav = dir.path().join("g.sav");
        let mut gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let mut slots = StateSlots::new(&rom);
        slots.save(&gb).unwrap();
        fs::write(&sav, vec![0x42; 0x2000]).unwrap();

        let msg = slots.load(&mut gb).unwrap();
        assert!(msg.contains("g.sav.bak"), "{msg}");
        assert_eq!(
            fs::read(dir.path().join("g.sav.bak")).unwrap(),
            vec![0x42; 0x2000]
        );

        fs::write(&sav, vec![0x43; 0x2000]).unwrap();
        assert_eq!(slots.load(&mut gb).unwrap(), "loaded state 1");
        assert!(
            !dir.path().join("g.sav.bak1").exists(),
            "only once per session"
        );
    }

    #[test]
    fn a_failed_load_still_backs_up_before_the_next_try() {
        let dir = tempfile::tempdir().unwrap();
        let rom = dir.path().join("g.gb");
        fs::write(dir.path().join("g.sav"), vec![1; 0x2000]).unwrap();
        fs::write(dir.path().join("g.ss1"), b"junk").unwrap();
        let mut gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let mut slots = StateSlots::new(&rom);
        assert!(slots.load(&mut gb).is_err());
        assert!(!slots.sav_backed_up);
    }
}
