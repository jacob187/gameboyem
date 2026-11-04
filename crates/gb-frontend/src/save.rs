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

/// Minimum time between "cannot write save" warnings while writes keep failing.
pub const WARN_INTERVAL: Duration = Duration::from_secs(60);

/// What to tell the user about a failed write.
#[derive(Debug, PartialEq, Eq)]
struct Warning {
    /// First failure since the last success (or since startup).
    first: bool,
    /// Failed attempts that were not reported since the previous warning.
    suppressed: u32,
}

/// Rate limiter for save-failure warnings: report the first failure at once,
/// then at most one warning per [`WARN_INTERVAL`] while failures continue.
/// A success resets it so the next failure is reported immediately again.
#[derive(Debug, Default)]
struct WarnLimiter {
    /// When the last warning was printed; `None` while writes are healthy.
    last_warned: Option<Instant>,
    suppressed: u32,
}

impl WarnLimiter {
    /// Record a failed write. `urgent` (the final flush on exit) always warns.
    fn failure(&mut self, now: Instant, urgent: bool) -> Option<Warning> {
        let due = match self.last_warned {
            None => true,
            Some(t) => now.saturating_duration_since(t) >= WARN_INTERVAL,
        };
        if !(due || urgent) {
            self.suppressed = self.suppressed.saturating_add(1);
            return None;
        }
        let warning = Warning {
            first: self.last_warned.is_none(),
            suppressed: std::mem::take(&mut self.suppressed),
        };
        self.last_warned = Some(now);
        Some(warning)
    }

    /// Record a successful write; true if writes had been failing.
    fn success(&mut self) -> bool {
        let was_failing = self.last_warned.is_some() || self.suppressed > 0;
        *self = Self::default();
        was_failing
    }
}

fn failure_message(path: &Path, err: &io::Error, w: &Warning) -> String {
    if w.first {
        format!(
            "warning: cannot write save {}: {err}\n  \
             SAVING IS FAILING: progress since the last successful save will be lost if the \
             emulator exits now. Retrying every {} s; this warning repeats at most every {} s.",
            path.display(),
            SAVE_INTERVAL.as_secs(),
            WARN_INTERVAL.as_secs()
        )
    } else {
        format!(
            "warning: still cannot write save {}: {err} ({} failed attempts since the last warning)",
            path.display(),
            w.suppressed
        )
    }
}

pub struct SaveManager {
    path: PathBuf,
    /// Battery RAM as of the last load/write; `None` until known.
    last: Option<Vec<u8>>,
    next_check: Instant,
    warn: WarnLimiter,
}

impl SaveManager {
    pub fn new(rom: &Path, now: Instant) -> Self {
        Self {
            path: save_path(rom),
            last: None,
            next_check: now + SAVE_INTERVAL,
            warn: WarnLimiter::default(),
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
            self.sync(gb.battery_ram(), now, false);
        }
    }

    /// Write now if battery RAM changed (e.g. on pause). Failures are rate-limited.
    pub fn flush(&mut self, gb: &GameBoy) {
        self.sync(gb.battery_ram(), Instant::now(), false);
    }

    /// Last write before exiting: a failure is always reported, never throttled,
    /// because it means data is being lost.
    pub fn flush_final(&mut self, gb: &GameBoy) {
        self.sync(gb.battery_ram(), Instant::now(), true);
    }

    /// Returns true if a file was written.
    fn sync(&mut self, ram: Option<Vec<u8>>, now: Instant, urgent: bool) -> bool {
        let Some(ram) = ram else { return false };
        if self.last.as_ref() == Some(&ram) {
            return false;
        }
        match write_atomic(&self.path, &ram) {
            Ok(()) => {
                self.last = Some(ram);
                if self.warn.success() {
                    eprintln!("save writing recovered: {}", self.path.display());
                }
                true
            }
            Err(e) => {
                if let Some(w) = self.warn.failure(now, urgent) {
                    eprintln!("{}", failure_message(&self.path, &e, &w));
                }
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync(m: &mut SaveManager, ram: Option<Vec<u8>>) -> bool {
        m.sync(ram, Instant::now(), false)
    }

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
        assert!(!sync(&mut m, Some(vec![0; 4])), "unchanged");
        assert!(!sync(&mut m, None), "no battery");
        assert!(!m.path().exists());
        assert!(sync(&mut m, Some(vec![1, 2, 3, 4])), "changed");
        assert_eq!(fs::read(m.path()).unwrap(), [1, 2, 3, 4]);
        assert!(!sync(&mut m, Some(vec![1, 2, 3, 4])), "already written");
    }

    #[test]
    fn failed_write_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let rom = dir.path().join("missing").join("g.gb");
        let mut m = SaveManager::new(&rom, Instant::now());
        assert!(!sync(&mut m, Some(vec![9])));
        fs::create_dir(dir.path().join("missing")).unwrap();
        assert!(sync(&mut m, Some(vec![9])));
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
            !sync(&mut m, gb.battery_ram()),
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

    fn secs(base: Instant, s: u64) -> Instant {
        base + Duration::from_secs(s)
    }

    #[test]
    fn limiter_warns_first_then_at_most_once_a_minute() {
        let t0 = Instant::now();
        let mut l = WarnLimiter::default();
        assert_eq!(
            l.failure(t0, false),
            Some(Warning {
                first: true,
                suppressed: 0
            })
        );
        // Retries every 5 s are silent for the next minute.
        for s in (5..60).step_by(5) {
            assert_eq!(l.failure(secs(t0, s), false), None, "t+{s}s");
        }
        // 11 suppressed (5..=55); the warning at +60 s reports them.
        assert_eq!(
            l.failure(secs(t0, 60), false),
            Some(Warning {
                first: false,
                suppressed: 11
            })
        );
        // The interval is measured from the last *warning*, not the last failure.
        assert_eq!(l.failure(secs(t0, 119), false), None);
        assert_eq!(
            l.failure(secs(t0, 120), false),
            Some(Warning {
                first: false,
                suppressed: 1
            })
        );
    }

    #[test]
    fn limiter_resets_after_success() {
        let t0 = Instant::now();
        let mut l = WarnLimiter::default();
        assert!(!l.success(), "healthy writes are not news");
        l.failure(t0, false);
        l.failure(secs(t0, 5), false);
        assert!(l.success(), "recovery after failures is reported");
        assert!(!l.success(), "only once");
        // A new failure right after recovery warns immediately and as a first.
        assert_eq!(
            l.failure(secs(t0, 6), false),
            Some(Warning {
                first: true,
                suppressed: 0
            })
        );
    }

    #[test]
    fn limiter_urgent_always_warns() {
        let t0 = Instant::now();
        let mut l = WarnLimiter::default();
        l.failure(t0, false);
        assert_eq!(l.failure(secs(t0, 1), false), None);
        assert_eq!(
            l.failure(secs(t0, 2), true),
            Some(Warning {
                first: false,
                suppressed: 1
            })
        );
        // The urgent warning restarts the interval.
        assert_eq!(l.failure(secs(t0, 3), false), None);
        assert!(l.failure(secs(t0, 62), false).is_some());
    }

    #[test]
    fn limiter_tolerates_clock_going_backwards() {
        let t0 = Instant::now();
        let mut l = WarnLimiter::default();
        l.failure(secs(t0, 10), false);
        assert_eq!(l.failure(t0, false), None);
    }

    #[test]
    fn failure_messages_state_that_saving_is_failing() {
        let e = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let path = Path::new("/ro/g.sav");
        let first = failure_message(
            path,
            &e,
            &Warning {
                first: true,
                suppressed: 0,
            },
        );
        assert!(first.contains("SAVING IS FAILING") && first.contains("/ro/g.sav"));
        assert!(first.contains("5 s") && first.contains("60 s"));
        let repeat = failure_message(
            path,
            &e,
            &Warning {
                first: false,
                suppressed: 11,
            },
        );
        assert!(repeat.contains("still cannot write") && repeat.contains("11 failed"));
    }

    #[test]
    fn manager_rate_limits_through_sync() {
        let dir = tempfile::tempdir().unwrap();
        let rom = dir.path().join("missing").join("g.gb");
        let t0 = Instant::now();
        let mut m = SaveManager::new(&rom, t0);
        assert!(!m.sync(Some(vec![1]), t0, false));
        assert!(m.warn.last_warned.is_some());
        assert!(!m.sync(Some(vec![1]), secs(t0, 5), false));
        assert_eq!(m.warn.suppressed, 1);
        fs::create_dir(dir.path().join("missing")).unwrap();
        assert!(m.sync(Some(vec![1]), secs(t0, 10), false));
        assert_eq!(m.warn.last_warned, None, "success resets the limiter");
    }

    /// A `.sav` that cannot be created (read-only directory) must warn, not crash.
    #[cfg(unix)]
    #[test]
    fn read_only_directory_does_not_crash() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let mut gb = GameBoy::new(battery_rom()).unwrap();
        fs::write(dir.path().join("g.sav"), b"short").unwrap(); // bad save: backup attempt too
        let rom = dir.path().join("g.gb");

        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let writable = fs::File::create(dir.path().join("probe")).is_ok();
        if !writable {
            let t0 = Instant::now();
            let mut m = SaveManager::new(&rom, t0);
            m.load(&mut gb); // wrong size -> .bak copy fails -> logged
            let mut ram = gb.battery_ram().unwrap();
            ram[0] = 1;
            assert!(!m.sync(Some(ram), t0, false));
            m.flush_final(&gb);
            let names: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names, vec![OsString::from("g.sav")], "no stray temp file");
        }
        // Restore so the temp dir can be removed.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        if writable {
            eprintln!("skipped: directory permissions are not enforced (running as root?)");
        }
    }
}
