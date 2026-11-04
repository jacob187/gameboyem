//! Battery-save persistence: `<rom>.sav` next to the ROM, written atomically.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gb_core::GameBoy;

/// How often battery RAM is compared against what is on disk.
pub const SAVE_INTERVAL: Duration = Duration::from_secs(5);

/// `game.gb` -> `game.sav`. If the ROM itself already ends in `.sav` the
/// extension is appended instead, so a save can never overwrite the ROM.
pub fn save_path(rom: &Path) -> PathBuf {
    let path = rom.with_extension("sav");
    if path == rom {
        append_ext(rom, "sav")
    } else {
        path
    }
}

fn append_ext(path: &Path, ext: &str) -> PathBuf {
    let mut s: OsString = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Write `data` to `path` via a temp file in the same directory + rename, so a
/// crash mid-write never leaves a truncated save.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let tmp = append_ext(path, "tmp");
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub struct SaveManager {
    path: PathBuf,
    /// Battery RAM as of the last load/write; `None` until known.
    last: Option<Vec<u8>>,
    next_check: Instant,
}

impl SaveManager {
    pub fn new(rom: &Path, now: Instant) -> Self {
        Self {
            path: save_path(rom),
            last: None,
            next_check: now + SAVE_INTERVAL,
        }
    }

    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }

    /// Load an existing save into the cartridge. Problems are logged, never fatal.
    pub fn load(&mut self, gb: &mut GameBoy) {
        if gb.battery_ram().is_none() {
            return; // no battery: nothing to load or save
        }
        match fs::read(&self.path) {
            Ok(data) => match gb.load_battery_ram(&data) {
                Ok(()) => eprintln!("loaded save {}", self.path.display()),
                Err(e) => {
                    // Keep the odd file around: the next write would replace it.
                    let backup = append_ext(&self.path, "bak");
                    match fs::copy(&self.path, &backup) {
                        Ok(_) => eprintln!(
                            "warning: ignoring {}: {e} (copied to {})",
                            self.path.display(),
                            backup.display()
                        ),
                        Err(c) => eprintln!(
                            "warning: ignoring {}: {e} (backup failed: {c})",
                            self.path.display()
                        ),
                    }
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("warning: cannot read {}: {e}", self.path.display()),
        }
        // Whatever the cartridge holds now is the baseline; only changes get written.
        self.last = gb.battery_ram();
    }

    /// Periodic check; writes if battery RAM changed since the last write.
    pub fn tick(&mut self, gb: &GameBoy, now: Instant) {
        if now >= self.next_check {
            self.next_check = now + SAVE_INTERVAL;
            self.flush(gb);
        }
    }

    /// Write now if battery RAM changed.
    pub fn flush(&mut self, gb: &GameBoy) {
        self.sync(gb.battery_ram());
    }

    /// Returns true if a file was written.
    fn sync(&mut self, ram: Option<Vec<u8>>) -> bool {
        let Some(ram) = ram else { return false };
        if self.last.as_ref() == Some(&ram) {
            return false;
        }
        match write_atomic(&self.path, &ram) {
            Ok(()) => {
                self.last = Some(ram);
                true
            }
            Err(e) => {
                eprintln!("warning: cannot write {}: {e}", self.path.display());
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_path_swaps_extension() {
        assert_eq!(
            save_path(Path::new("/roms/Tetris.gb")),
            PathBuf::from("/roms/Tetris.sav")
        );
        assert_eq!(save_path(Path::new("a.b.gb")), PathBuf::from("a.b.sav"));
        assert_eq!(save_path(Path::new("noext")), PathBuf::from("noext.sav"));
        assert_eq!(save_path(Path::new("x.GBC")), PathBuf::from("x.sav"));
    }

    #[test]
    fn save_never_aliases_the_rom() {
        assert_eq!(save_path(Path::new("x.sav")), PathBuf::from("x.sav.sav"));
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.sav");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two!").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two!");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from("g.sav")]);
    }

    #[test]
    fn atomic_write_to_missing_dir_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        assert!(write_atomic(&dir.path().join("nope").join("g.sav"), b"x").is_err());
    }

    #[test]
    fn sync_writes_only_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let rom = dir.path().join("g.gb");
        let mut m = SaveManager::new(&rom, Instant::now());
        m.last = Some(vec![0; 4]);
        assert!(!m.sync(Some(vec![0; 4])), "unchanged");
        assert!(!m.sync(None), "no battery");
        assert!(!m.path().exists());
        assert!(m.sync(Some(vec![1, 2, 3, 4])), "changed");
        assert_eq!(fs::read(m.path()).unwrap(), [1, 2, 3, 4]);
        assert!(!m.sync(Some(vec![1, 2, 3, 4])), "already written");
    }

    #[test]
    fn failed_write_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let rom = dir.path().join("missing").join("g.gb");
        let mut m = SaveManager::new(&rom, Instant::now());
        assert!(!m.sync(Some(vec![9])));
        fs::create_dir(dir.path().join("missing")).unwrap();
        assert!(m.sync(Some(vec![9])));
    }

    /// 32 KiB MBC1+RAM+BATTERY ROM with 8 KiB RAM.
    fn battery_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x147] = 0x03;
        rom[0x148] = 0x00;
        rom[0x149] = 0x02;
        rom
    }

    #[test]
    fn load_applies_valid_save_and_backs_up_bad_one() {
        let dir = tempfile::tempdir().unwrap();
        let rom_path = dir.path().join("g.gb");

        // Valid save.
        let mut gb = GameBoy::new(battery_rom()).unwrap();
        let mut ram = gb.battery_ram().expect("battery");
        ram[0] = 0xAB;
        fs::write(dir.path().join("g.sav"), &ram).unwrap();
        let mut m = SaveManager::new(&rom_path, Instant::now());
        m.load(&mut gb);
        assert_eq!(gb.battery_ram().unwrap()[0], 0xAB);
        assert!(
            !m.sync(gb.battery_ram()),
            "freshly loaded save is not rewritten"
        );

        // Wrong-size save: ignored, original preserved as .bak, RAM untouched.
        let mut gb = GameBoy::new(battery_rom()).unwrap();
        fs::write(dir.path().join("g.sav"), b"short").unwrap();
        let mut m = SaveManager::new(&rom_path, Instant::now());
        m.load(&mut gb);
        assert_eq!(gb.battery_ram().unwrap()[0], 0);
        assert_eq!(fs::read(dir.path().join("g.sav.bak")).unwrap(), b"short");
    }

    #[test]
    fn rom_without_battery_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        let rom_path = dir.path().join("g.gb");
        let mut gb = GameBoy::new(vec![0u8; 0x8000]).unwrap();
        let mut m = SaveManager::new(&rom_path, Instant::now());
        m.load(&mut gb);
        m.flush(&gb);
        assert!(!m.path().exists());
    }
}
