//! Battery-save persistence: `<rom>.sav` next to the ROM, written atomically.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
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
///
/// This blocks (`sync_all`) and is deliberately called on the event-loop thread.
/// Measured on macOS with the real `fsync` (20 writes each): 8 KiB median 4.0 ms,
/// max 18 ms; 128 KiB + footer median 4.5 ms, max 5 ms. That is under the 40 ms
/// audio prefill and the 67 ms frame-pacer catch-up cap, and it happens at most
/// once per [`SAVE_INTERVAL`], only after RAM changed. Pause and exit must write
/// synchronously anyway, and a background writer would add states (in-flight
/// write, retry, rate-limited warnings, what `last` means) for a ~1 frame stall.
/// Revisit if a slow disk shows up in practice.
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

/// Largest valid save file: 128 KiB of cart RAM (MBC5, RAM code 4) plus the
/// 48-byte MBC3 RTC footer. Anything bigger cannot be a save for any cartridge.
/// (The core exports no such constant; a test checks this still covers it.)
const MAX_SAVE_BYTES: u64 = 0x20000 + 48;

/// Read the save at `path`: `Ok(None)` if there is none. Rejects, without
/// reading the contents, anything that is not a regular file (a FIFO would block
/// `open`, `/dev/zero` never ends), and stops at [`MAX_SAVE_BYTES`] + 1.
fn read_save(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !meta.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let mut data = Vec::new();
    fs::File::open(path)?
        .take(MAX_SAVE_BYTES + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > MAX_SAVE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("larger than any valid save ({MAX_SAVE_BYTES} bytes)"),
        ));
    }
    Ok(Some(data))
}

/// Highest numbered backup tried before giving up (`.bak`, `.bak1`, ...).
const MAX_BACKUPS: u32 = 1000;

/// Copy `src` to `<src>.bak`, or the first free `<src>.bak1`, `.bak2`, ... so an
/// existing backup is never overwritten (two ROMs can share a save stem). If an
/// existing backup already holds identical bytes it is reused, so relaunching
/// with the same odd file does not pile up copies. Returns the backup's path.
fn backup_without_overwrite(src: &Path) -> io::Result<PathBuf> {
    let data = fs::read(src)?;
    for n in 0..=MAX_BACKUPS {
        let candidate = if n == 0 {
            append_ext(src, "bak")
        } else {
            append_ext(src, &format!("bak{n}"))
        };
        // `create_new` makes "does not exist yet" and "claim it" one atomic step.
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut f) => {
                let written = f.write_all(&data).and_then(|()| f.sync_all());
                drop(f);
                if let Err(e) = written {
                    let _ = fs::remove_file(&candidate);
                    return Err(e);
                }
                return Ok(candidate);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if fs::read(&candidate).is_ok_and(|existing| existing == data) {
                    return Ok(candidate);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{MAX_BACKUPS} backups of {} already exist", src.display()),
    ))
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

const BLOCKED_NOTE: &str = "The file was left untouched and SAVING IS DISABLED for this session \
so it is not overwritten. Move or fix it and restart.";

pub struct SaveManager {
    path: PathBuf,
    /// Battery RAM as of the last load/write; `None` until known.
    last: Option<Vec<u8>>,
    next_check: Instant,
    warn: WarnLimiter,
    /// Set when an existing file at `path` could not be read or backed up. Writing
    /// would replace it (rename ignores its permissions), so nothing is written.
    write_blocked: bool,
}

impl SaveManager {
    pub fn new(rom: &Path, now: Instant) -> Self {
        Self {
            path: save_path(rom),
            last: None,
            next_check: now + SAVE_INTERVAL,
            warn: WarnLimiter::default(),
            write_blocked: false,
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
        match read_save(&self.path) {
            Ok(Some(data)) => match gb.load_battery_ram(&data) {
                Ok(()) => eprintln!("loaded save {}", self.path.display()),
                Err(e) => {
                    // Keep the odd file around: the next write would replace it.
                    match backup_without_overwrite(&self.path) {
                        Ok(backup) => eprintln!(
                            "warning: ignoring {}: {e} (copied to {})",
                            self.path.display(),
                            backup.display()
                        ),
                        Err(c) => {
                            self.write_blocked = true;
                            eprintln!(
                                "warning: ignoring {}: {e} (backup failed: {c})\n  {BLOCKED_NOTE}",
                                self.path.display()
                            );
                        }
                    }
                }
            },
            Ok(None) => {}
            Err(e) => {
                // Unreadable, not a regular file, or oversized: no copy is made,
                // so the file must not be replaced either.
                self.write_blocked = true;
                eprintln!(
                    "warning: cannot use save {}: {e}\n  {BLOCKED_NOTE}",
                    self.path.display()
                );
            }
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

    /// Does `ram` differ from what is on disk (or was loaded)? The one place that
    /// decides "changed"; a core-provided generation counter could replace it.
    fn needs_write(&self, ram: &[u8]) -> bool {
        self.last.as_deref() != Some(ram)
    }

    /// Returns true if a file was written.
    fn sync(&mut self, ram: Option<Vec<u8>>, now: Instant, urgent: bool) -> bool {
        let Some(ram) = ram else { return false };
        if !self.needs_write(&ram) {
            return false;
        }
        if self.write_blocked {
            // Said once at load; on exit say what that means for this session.
            if urgent {
                eprintln!(
                    "warning: progress NOT saved: {} was not usable at startup and was left untouched",
                    self.path.display()
                );
            }
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
    fn backup_uses_bak_then_next_free_name_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let sav = dir.path().join("g.sav");
        fs::write(&sav, b"first").unwrap();
        let b0 = backup_without_overwrite(&sav).unwrap();
        assert_eq!(b0, dir.path().join("g.sav.bak"));

        fs::write(&sav, b"second").unwrap();
        let b1 = backup_without_overwrite(&sav).unwrap();
        assert_eq!(b1, dir.path().join("g.sav.bak1"));

        fs::write(&sav, b"third").unwrap();
        let b2 = backup_without_overwrite(&sav).unwrap();
        assert_eq!(b2, dir.path().join("g.sav.bak2"));

        assert_eq!(fs::read(&b0).unwrap(), b"first");
        assert_eq!(fs::read(&b1).unwrap(), b"second");
        assert_eq!(fs::read(&b2).unwrap(), b"third");
    }

    #[test]
    fn backup_reuses_an_identical_existing_backup() {
        let dir = tempfile::tempdir().unwrap();
        let sav = dir.path().join("g.sav");
        fs::write(&sav, b"odd").unwrap();
        let first = backup_without_overwrite(&sav).unwrap();
        assert_eq!(backup_without_overwrite(&sav).unwrap(), first);
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            2,
            "sav + one bak"
        );
    }

    #[test]
    fn backup_of_missing_file_fails_without_creating_one() {
        let dir = tempfile::tempdir().unwrap();
        assert!(backup_without_overwrite(&dir.path().join("none.sav")).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn load_with_wrong_size_save_keeps_prior_backup() {
        let dir = tempfile::tempdir().unwrap();
        let rom_path = dir.path().join("g.gb");
        let mut rom = vec![0u8; 0x8000];
        rom[0x147] = 0x03; // MBC1+RAM+BATTERY
        rom[0x149] = 0x02; // 8 KiB
        let mut gb = GameBoy::new(rom).unwrap();
        let sav = save_path(&rom_path);
        fs::write(sav.with_extension("sav.bak"), b"other game's save").unwrap();
        fs::write(&sav, b"wrong size").unwrap();

        SaveManager::new(&rom_path, Instant::now()).load(&mut gb);

        assert_eq!(
            fs::read(dir.path().join("g.sav.bak")).unwrap(),
            b"other game's save"
        );
        assert_eq!(
            fs::read(dir.path().join("g.sav.bak1")).unwrap(),
            b"wrong size"
        );
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

    /// A cartridge with `cart_type` and RAM size code `ram_code`, 32 KiB ROM.
    fn cart(cart_type: u8, ram_code: u8) -> GameBoy {
        let mut rom = vec![0u8; 0x8000];
        rom[0x147] = cart_type;
        rom[0x149] = ram_code;
        GameBoy::new(rom).unwrap()
    }

    fn dir_names(dir: &Path) -> Vec<OsString> {
        let mut v: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    }

    /// Load `contents` as `g.sav`, change RAM, and try every kind of write.
    /// Returns the manager (for inspection) after asserting nothing was written.
    fn load_then_try_to_save(dir: &Path, gb: &mut GameBoy) -> SaveManager {
        let t0 = Instant::now();
        let mut m = SaveManager::new(&dir.join("g.gb"), t0);
        m.load(gb);
        assert!(m.write_blocked);
        let mut ram = gb.battery_ram().unwrap();
        ram[0] = 0x55;
        assert!(!m.sync(Some(ram.clone()), t0, false));
        gb.load_battery_ram(&ram).unwrap();
        m.flush(gb);
        m.tick(gb, secs(t0, 10));
        m.flush_final(gb);
        m
    }

    #[test]
    fn oversized_save_is_rejected_without_copy_or_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let big = vec![7u8; MAX_SAVE_BYTES as usize + 1];
        fs::write(dir.path().join("g.sav"), &big).unwrap();
        let mut gb = GameBoy::new(battery_rom()).unwrap();

        load_then_try_to_save(dir.path(), &mut gb);

        assert_eq!(
            dir_names(dir.path()),
            [OsString::from("g.sav")],
            "no .bak, no .tmp"
        );
        assert_eq!(fs::read(dir.path().join("g.sav")).unwrap(), big);
    }

    #[test]
    fn read_save_stops_at_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("g.sav");
        fs::write(&p, vec![1u8; MAX_SAVE_BYTES as usize]).unwrap();
        assert_eq!(read_save(&p).unwrap().unwrap().len() as u64, MAX_SAVE_BYTES);
        fs::write(&p, vec![1u8; MAX_SAVE_BYTES as usize + 1]).unwrap();
        assert_eq!(
            read_save(&p).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(read_save(&dir.path().join("none.sav")).unwrap().is_none());
    }

    #[test]
    fn directory_at_the_save_path_blocks_writes() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("g.sav")).unwrap();
        let mut gb = GameBoy::new(battery_rom()).unwrap();

        load_then_try_to_save(dir.path(), &mut gb);

        assert!(dir.path().join("g.sav").is_dir());
        assert_eq!(dir_names(dir.path()), [OsString::from("g.sav")]);
    }

    /// Unreadable save (mode 000): the first RAM change must not rename over it.
    #[cfg(unix)]
    #[test]
    fn unreadable_save_is_never_replaced() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sav = dir.path().join("g.sav");
        fs::write(&sav, b"precious progress").unwrap();
        fs::set_permissions(&sav, fs::Permissions::from_mode(0o000)).unwrap();
        let readable = fs::File::open(&sav).is_ok();
        if !readable {
            let mut gb = GameBoy::new(battery_rom()).unwrap();
            load_then_try_to_save(dir.path(), &mut gb);
            assert_eq!(
                dir_names(dir.path()),
                [OsString::from("g.sav")],
                "no .bak, no .tmp"
            );
        }
        // Restore so the content can be checked and the temp dir removed.
        fs::set_permissions(&sav, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(fs::read(&sav).unwrap(), b"precious progress");
        if readable {
            eprintln!("skipped: file permissions are not enforced (running as root?)");
        }
    }

    #[test]
    fn max_save_bytes_covers_the_largest_cartridges() {
        // MBC5 with 128 KiB RAM; MBC3 + RTC with 32 KiB RAM (+ footer).
        for (cart_type, ram_code) in [(0x1B, 0x04), (0x10, 0x03)] {
            let mut gb = cart(cart_type, ram_code);
            let mut ram = gb.battery_ram().expect("battery");
            assert!(ram.len() as u64 <= MAX_SAVE_BYTES, "type {cart_type:#x}");
            // A save of exactly the largest valid size loads, and is not blocked.
            ram[0] = 0xAB;
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("g.sav"), &ram).unwrap();
            let mut m = SaveManager::new(&dir.path().join("g.gb"), Instant::now());
            m.load(&mut gb);
            assert!(!m.write_blocked);
            assert_eq!(gb.battery_ram().unwrap()[0], 0xAB);
        }
        assert_eq!(cart(0x1B, 0x04).battery_ram().unwrap().len(), 0x20000);
    }

    #[test]
    fn missing_save_does_not_block_writes() {
        let dir = tempfile::tempdir().unwrap();
        let mut gb = GameBoy::new(battery_rom()).unwrap();
        let t0 = Instant::now();
        let mut m = SaveManager::new(&dir.path().join("g.gb"), t0);
        m.load(&mut gb);
        assert!(!m.write_blocked);
        let mut ram = gb.battery_ram().unwrap();
        ram[0] = 1;
        assert!(m.sync(Some(ram), t0, false));
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
            assert!(
                m.write_blocked,
                "could not back it up, so must not replace it"
            );
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
