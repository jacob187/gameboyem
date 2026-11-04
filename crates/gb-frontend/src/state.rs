//! Save-state slots: `<rom>.ss1` ... `<rom>.ss9` next to the ROM, written
//! atomically. Loading a state also rewinds battery RAM, which the battery
//! saver then writes to `<rom>.sav`, so the first load of a session first
//! backs that file up.

use std::path::{Path, PathBuf};

use gb_core::GameBoy;

use crate::save::{append_ext, backup_without_overwrite, read_capped, save_path, write_atomic};

pub const FIRST_SLOT: u8 = 1;
pub const LAST_SLOT: u8 = 9;

/// Larger than any state the core produces. The fixed machine state is about
/// 62 KiB on a DMG and 139 KiB in CGB colour mode (both VRAM and WRAM banks plus
/// the RGB555 back and front frames), plus at most 128 KiB of cart RAM: 267 KiB
/// at worst, which is why this is 512 KiB and not 256 KiB. A test checks the
/// largest cases.
const MAX_STATE_BYTES: u64 = 512 * 1024;

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
    pub fn save(&self, gb: &GameBoy) -> Result<String, String> {
        let path = self.path();
        write_atomic(&path, &gb.save_state())
            .map(|()| format!("saved state {}", self.slot))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// Load the current slot into `gb`. On error `gb` is unchanged.
    pub fn load(&mut self, gb: &mut GameBoy) -> Result<String, String> {
        let path = self.path();
        let data = match read_capped(&path, MAX_STATE_BYTES, "save state") {
            Ok(Some(d)) => d,
            Ok(None) => return Err(format!("state {} is empty", self.slot)),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let backup = self.backup_sav_once()?;
        gb.load_state(&data)
            .map_err(|e| format!("cannot load state {}: {e}", self.slot))?;
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
    use gb_core::Model;
    use std::fs;

    fn battery_rom(ram_code: u8) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x147] = 0x1B; // MBC5+RAM+BATTERY
        rom[0x149] = ram_code;
        rom
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
    fn largest_state_fits_the_cap() {
        let gb = GameBoy::new(battery_rom(0x04)).unwrap(); // 128 KiB RAM
        let len = gb.save_state().len() as u64;
        assert!(len <= MAX_STATE_BYTES, "{len}");
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
        for (ram_code, ram_len) in [(0x00, 0), (0x04, 128 * 1024)] {
            let gb = GameBoy::with_model(cgb_rom(ram_code), Model::Cgb).unwrap();
            assert!(gb.cgb_framebuffer().is_some(), "colour mode");
            let len = gb.save_state().len() as u64;
            assert!(len < MAX_STATE_BYTES, "{len}");
            assert!(len < 150_000 + ram_len, "{len}: the fixed part grew");
        }
        let largest = GameBoy::with_model(cgb_rom(0x04), Model::Cgb)
            .unwrap()
            .save_state()
            .len() as u64;
        assert!(
            MAX_STATE_BYTES - largest >= 100 * 1024,
            "margin {}",
            MAX_STATE_BYTES - largest
        );
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
            vec![0; MAX_STATE_BYTES as usize + 1],
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
