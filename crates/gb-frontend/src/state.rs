//! The app's own save states of a game, in `states/<game-id>/` (in the app folder, see
//! `library.rs`), every file written atomically:
//!
//! - `resume.state`: written when the player leaves the game and every minute of play; the
//!   next start continues from it.
//! - `snap-<unix-secs>.state` and `snap-<unix-secs>.png`: a snapshot the player took, and
//!   the picture on screen when they did. The time is the name; there are no numbers.
//!
//! Loading a state also rewinds battery RAM, which the battery saver then writes to the
//! game's `.sav`, so a load that could cost progress first backs that file up.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use emu_common::{Core, StateError, System};
use gb_core::Model;

use crate::library::{GamePaths, RESUME_FILE, now_secs};
use crate::save::{append_ext, backup_without_overwrite, read_capped, write_atomic};

/// Snapshots kept per game; one more makes the oldest go.
pub const MAX_SNAPSHOTS: usize = 50;

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

/// Largest snapshot picture read back: a GBA frame is far smaller than this.
pub const MAX_IMAGE_BYTES: u64 = 1024 * 1024;

/// `game.gb` -> `game.ss<slot>`: where the oldest versions kept numbered slots, next to
/// the ROM, which is now only read to turn them into snapshots. Like the battery save, the
/// extension is appended instead if that would name the ROM itself.
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
pub fn load_error(e: &StateError, system: System, variant: u8) -> String {
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

/// What saving reports for a core that has no state format ([`Core::supports_save_state`]).
const NO_STATES: &str = "this core has no save states";

// ---------------------------------------------------------------------------
// File names
// ---------------------------------------------------------------------------

/// The time in a snapshot file name `snap-<unix-secs>.<ext>`; `None` for any other name
/// (a `.tmp` left by a crash, a `.bad`, a stray file).
fn snapshot_secs(name: &str, ext: &str) -> Option<u64> {
    let digits = name.strip_prefix("snap-")?.strip_suffix(ext)?;
    let digits = digits.strip_suffix('.')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Whether `name` is a file this module writes into a states folder (also the leftovers of
/// an interrupted write), so removing a game can delete exactly those.
pub fn is_state_file_name(name: &str) -> bool {
    let name = name.strip_suffix(".tmp").unwrap_or(name);
    name == RESUME_FILE
        || name.strip_prefix(RESUME_FILE) == Some(".bad")
        || snapshot_secs(name, "state").is_some()
        || snapshot_secs(name, "png").is_some()
}

/// A snapshot on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// When it was taken, in Unix seconds; also what names its files.
    pub secs: u64,
    /// The picture, if the file is there.
    pub image: Option<PathBuf>,
}

/// What [`StateStore::save_snapshot`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotSaved {
    pub secs: u64,
    /// Oldest snapshots removed to stay within [`MAX_SNAPSHOTS`].
    pub evicted: usize,
}

/// What [`StateStore::load_resume`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Resume {
    /// There is no resume point; the game was left as powered on.
    Absent,
    /// The game continues from it. `backup` is where the game's own save was copied first,
    /// when the resume point was older than that save.
    Loaded { backup: Option<PathBuf> },
    /// It could not be used; the game was left as powered on. `kept` is where the file was
    /// moved to (`resume.state.bad`), if that worked.
    Failed { why: String, kept: Option<PathBuf> },
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// The resume point and snapshots of the running game.
pub struct StateStore {
    paths: GamePaths,
    /// The `.sav` has been backed up (or there was none) this session.
    sav_backed_up: bool,
}

impl StateStore {
    pub fn new(paths: GamePaths) -> Self {
        Self {
            paths,
            sav_backed_up: false,
        }
    }

    /// Write the resume point (replacing the last one).
    pub fn save_resume(&self, core: &dyn Core) -> Result<(), String> {
        if !core.supports_save_state() {
            return Err(NO_STATES.to_owned());
        }
        let path = self.paths.resume_state();
        self.write(&path, &core.save_state())
    }

    /// Forget the resume point (Start fresh, Restart). A missing one is fine.
    pub fn discard_resume(&self) {
        match fs::remove_file(self.paths.resume_state()) {
            Ok(()) => log::info!("discarded the resume point"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("cannot discard the resume point: {e}"),
        }
    }

    /// Continue from the resume point into `core`, which has just been powered on with
    /// its battery save. A resume point that cannot be loaded is moved to
    /// `resume.state.bad`, and `core` is left as it was.
    pub fn load_resume(&mut self, core: &mut dyn Core) -> Resume {
        if !core.supports_save_state() {
            return Resume::Absent;
        }
        let path = self.paths.resume_state();
        let data = match read_capped(&path, max_state_bytes(core.system()), "resume point") {
            Ok(Some(data)) => data,
            Ok(None) => return Resume::Absent,
            Err(e) => return self.set_aside(&path, format!("cannot read it: {e}")),
        };
        // A resume point is at most a minute older than the game's save when the program
        // ended cleanly (the save is written first), so a newer save means a crash after
        // the game saved: loading would rewind that save, so keep a copy of it.
        let backup = if modified(&self.paths.sav) > modified(&path) {
            match self.backup_sav_once() {
                Ok(backup) => backup,
                // The file is fine; do not set it aside.
                Err(why) => return Resume::Failed { why, kept: None },
            }
        } else {
            None
        };
        match core.load_state(&data) {
            Ok(()) => {
                self.sav_backed_up |= backup.is_some();
                Resume::Loaded { backup }
            }
            Err(e) => {
                let why = load_error(&e, core.system(), core.hw_variant());
                self.set_aside(&path, why)
            }
        }
    }

    /// Keep an unusable resume point as `resume.state.bad` instead of deleting it.
    fn set_aside(&self, path: &Path, why: String) -> Resume {
        let bad = self.paths.resume_bad();
        match fs::rename(path, &bad) {
            Ok(()) => Resume::Failed {
                why,
                kept: Some(bad),
            },
            Err(e) => {
                log::warn!("cannot keep {} as {}: {e}", path.display(), bad.display());
                Resume::Failed { why, kept: None }
            }
        }
    }

    /// The snapshots, newest first.
    pub fn snapshots(&self) -> Vec<Snapshot> {
        list_snapshots(&self.paths)
    }

    /// Add a snapshot of `core` taken at `now` (Unix seconds), with the picture `png` if
    /// there is one (a picture that cannot be written is only logged). Past
    /// [`MAX_SNAPSHOTS`] the oldest are removed.
    pub fn save_snapshot(
        &self,
        core: &dyn Core,
        png: Option<&[u8]>,
        now: u64,
    ) -> Result<SnapshotSaved, String> {
        if !core.supports_save_state() {
            return Err(NO_STATES.to_owned());
        }
        // Never the name of an existing snapshot, and never older than the newest, so two
        // saves in one second (or a clock set back) still list in the order they were made.
        let newest = list_snapshots(&self.paths).first().map(|s| s.secs);
        let secs = newest.map_or(now, |n| now.max(n.saturating_add(1)));
        self.write(&self.paths.snapshot_state(secs), &core.save_state())?;
        if let Some(png) = png {
            let path = self.paths.snapshot_image(secs);
            if let Err(e) = write_atomic(&path, png) {
                log::warn!("cannot write {}: {e}", path.display());
            }
        }
        Ok(SnapshotSaved {
            secs,
            evicted: prune_snapshots(&self.paths),
        })
    }

    /// Load the snapshot taken at `secs` into `core`. On error `core` is unchanged. The
    /// first load of a session copies the `.sav` aside; the path of the copy is returned.
    pub fn load_snapshot(
        &mut self,
        core: &mut dyn Core,
        secs: u64,
    ) -> Result<Option<PathBuf>, String> {
        if !core.supports_save_state() {
            return Err(NO_STATES.to_owned());
        }
        let path = self.paths.snapshot_state(secs);
        let data = match read_capped(&path, max_state_bytes(core.system()), "snapshot") {
            Ok(Some(d)) => d,
            Ok(None) => return Err("that snapshot is gone".to_owned()),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let backup = self.backup_sav_once()?;
        core.load_state(&data).map_err(|e| {
            format!(
                "cannot load the snapshot: {}",
                load_error(&e, core.system(), core.hw_variant())
            )
        })?;
        self.sav_backed_up = true;
        Ok(backup)
    }

    /// Delete the snapshot taken at `secs`, picture included.
    pub fn delete_snapshot(&self, secs: u64) {
        remove_snapshot(&self.paths, secs);
    }

    fn write(&self, path: &Path, data: &[u8]) -> Result<(), String> {
        fs::create_dir_all(self.paths.states_dir())
            .and_then(|()| write_atomic(path, data))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// Before the first load of a session that could rewind it, copy the game's `.sav`
    /// aside: the loaded state's battery RAM replaces it on the next autosave. Refuses the
    /// load if the copy fails, so real progress is never silently overwritten.
    fn backup_sav_once(&self) -> Result<Option<PathBuf>, String> {
        let sav = &self.paths.sav;
        if self.sav_backed_up || !sav.is_file() {
            return Ok(None);
        }
        backup_without_overwrite(sav)
            .map(Some)
            .map_err(|e| format!("not loading: cannot back up {} first: {e}", sav.display()))
    }
}

fn modified(path: &Path) -> Option<std::time::SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn list_snapshots(paths: &GamePaths) -> Vec<Snapshot> {
    let Ok(listing) = fs::read_dir(paths.states_dir()) else {
        return Vec::new();
    };
    let mut found: Vec<Snapshot> = listing
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| snapshot_secs(&e.file_name().to_string_lossy(), "state"))
        .map(|secs| {
            let image = paths.snapshot_image(secs);
            Snapshot {
                secs,
                image: image.is_file().then_some(image),
            }
        })
        .collect();
    found.sort_by_key(|s| std::cmp::Reverse(s.secs));
    found
}

fn remove_snapshot(paths: &GamePaths, secs: u64) {
    for file in [paths.snapshot_state(secs), paths.snapshot_image(secs)] {
        match fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("cannot delete {}: {e}", file.display()),
        }
    }
}

/// Remove the oldest snapshots beyond [`MAX_SNAPSHOTS`]; returns how many went.
fn prune_snapshots(paths: &GamePaths) -> usize {
    let all = list_snapshots(paths);
    let extra = all.len().saturating_sub(MAX_SNAPSHOTS);
    for old in all.iter().rev().take(extra) {
        log::info!(
            "removing the oldest snapshot ({} kept at most)",
            MAX_SNAPSHOTS
        );
        remove_snapshot(paths, old.secs);
    }
    extra
}

// ---------------------------------------------------------------------------
// The numbered slots of older versions
// ---------------------------------------------------------------------------

/// The first free snapshot time at or after `secs`.
fn free_secs(paths: &GamePaths, mut secs: u64) -> u64 {
    while paths.snapshot_state(secs).exists() {
        secs += 1;
    }
    secs
}

/// Turn the numbered slots of older versions into snapshots, with the slot file's modified
/// time as the time and no picture, and log each one. Returns how many were made.
///
/// Slot files in the app folder (`states/<id>.ssN`) are the app's own and are moved. A
/// `<rom>.ssN` beside the ROM is copied and left in place, and only while the game has no
/// states folder yet, so it is not imported again at every start. (With no app folder the
/// slots beside the ROM are the app's own files, and are treated as beside ones.)
pub fn migrate_legacy_states(rom: &Path, paths: &GamePaths) -> usize {
    let first_time = fs::symlink_metadata(paths.states_dir()).is_err();
    let cap = max_state_bytes(System::Gb).max(max_state_bytes(System::Gba));
    let mut made = 0;
    for slot in 1..=9 {
        let own = paths.legacy_state(slot);
        let beside = state_path(rom, slot);
        let (from, moved) = if own != beside && own.is_file() {
            (own, true)
        } else if first_time && beside.is_file() {
            (beside, false)
        } else {
            continue;
        };
        let data = match read_capped(&from, cap, "save state") {
            Ok(Some(data)) => data,
            Ok(None) => continue,
            Err(e) => {
                log::warn!("not converting save state {}: {e}", from.display());
                continue;
            }
        };
        let when = modified(&from)
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or_else(now_secs, |d| d.as_secs());
        let secs = free_secs(paths, when);
        let target = paths.snapshot_state(secs);
        let written =
            fs::create_dir_all(paths.states_dir()).and_then(|()| write_atomic(&target, &data));
        match written {
            Ok(()) => {
                log::info!(
                    "converted save state {} to the snapshot {}{}",
                    from.display(),
                    target.display(),
                    if moved {
                        ""
                    } else {
                        " (the original is left as it was)"
                    }
                );
                made += 1;
                if moved {
                    let _ = fs::remove_file(&from);
                }
            }
            Err(e) => log::warn!(
                "cannot convert save state {} to {}: {e}",
                from.display(),
                target.display()
            ),
        }
    }
    prune_snapshots(paths);
    made
}

#[cfg(test)]
mod tests {
    use super::*;
    use gb_core::{GameBoy, Model};

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

    fn set_mtime(path: &Path, secs: u64) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(secs))
            .unwrap();
    }

    fn store_in(dir: &Path) -> StateStore {
        StateStore::new(GamePaths::beside(&dir.join("g.gb")))
    }

    #[test]
    fn a_core_without_states_is_not_asked_for_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        let mut core = NoStates(GameBoy::new(battery_rom(0x02)).unwrap());
        assert_eq!(store.save_resume(&core), Err(NO_STATES.to_owned()));
        assert_eq!(
            store.save_snapshot(&core, None, 5),
            Err(NO_STATES.to_owned())
        );
        assert_eq!(store.load_snapshot(&mut core, 5), Err(NO_STATES.to_owned()));
        assert_eq!(store.load_resume(&mut core), Resume::Absent);
        assert!(!store.paths.states_dir().exists(), "nothing was written");
    }

    #[test]
    fn only_our_file_names_are_recognised() {
        assert_eq!(
            snapshot_secs("snap-1700000000.state", "state"),
            Some(1_700_000_000)
        );
        assert_eq!(snapshot_secs("snap-17.png", "png"), Some(17));
        for bad in [
            "snap-.state",
            "snap-1x.state",
            "snap--1.state",
            "snap-1.state.tmp",
            "snap-1.state.bad",
            "snap-99999999999999999999999.state",
            "resume.state",
            "snap1.state",
        ] {
            assert_eq!(snapshot_secs(bad, "state"), None, "{bad}");
        }
        for ours in [
            "resume.state",
            "resume.state.bad",
            "resume.state.tmp",
            "snap-5.state",
            "snap-5.png",
            "snap-5.state.tmp",
        ] {
            assert!(is_state_file_name(ours), "{ours}");
        }
        for other in [
            "notes.txt",
            "snap-x.state",
            "resume.state.bad.tmp2",
            "g.sav",
        ] {
            assert!(!is_state_file_name(other), "{other}");
        }
    }

    #[test]
    fn a_snapshot_stores_the_state_the_picture_and_the_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let mut store = store_in(dir.path());
        gb.step_frame();
        let snap = gb.save_state();
        let saved = store.save_snapshot(&gb, Some(b"png"), 1_000).unwrap();
        assert_eq!(
            saved,
            SnapshotSaved {
                secs: 1_000,
                evicted: 0
            }
        );
        let paths = &store.paths;
        assert_eq!(fs::read(paths.snapshot_state(1_000)).unwrap(), snap);
        assert_eq!(fs::read(paths.snapshot_image(1_000)).unwrap(), b"png");
        assert!(
            paths
                .snapshot_state(1_000)
                .starts_with(dir.path().join("g.states"))
        );

        for _ in 0..10 {
            gb.step_frame();
        }
        assert_ne!(gb.save_state(), snap);
        assert_eq!(store.load_snapshot(&mut gb, 1_000), Ok(None));
        assert_eq!(gb.save_state(), snap);
    }

    #[test]
    fn snapshots_list_newest_first_and_a_missing_picture_is_a_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let store = store_in(dir.path());
        assert!(store.snapshots().is_empty(), "no folder yet");
        store.save_snapshot(&gb, Some(b"a"), 300).unwrap();
        store.save_snapshot(&gb, None, 100).unwrap(); // older than the newest: still after it
        store.save_snapshot(&gb, Some(b"c"), 500).unwrap();
        // Strays are not snapshots.
        let strays = store.paths.states_dir();
        fs::write(strays.join("snap-9.state.tmp"), b"x").unwrap();
        fs::write(strays.join("notes.txt"), b"x").unwrap();
        fs::write(strays.join("snap-7.png"), b"x").unwrap();
        let list = store.snapshots();
        let secs: Vec<u64> = list.iter().map(|s| s.secs).collect();
        assert_eq!(secs, [500, 301, 300]);
        assert!(list[1].image.is_none(), "saved without a picture");
        assert!(list[0].image.is_some() && list[2].image.is_some());
    }

    #[test]
    fn two_saves_in_one_second_do_not_replace_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let store = store_in(dir.path());
        for _ in 0..3 {
            store.save_snapshot(&gb, None, 42).unwrap();
        }
        let secs: Vec<u64> = store.snapshots().iter().map(|s| s.secs).collect();
        assert_eq!(secs, [44, 43, 42]);
    }

    #[test]
    fn deleting_a_snapshot_removes_both_files_and_only_that_one() {
        let dir = tempfile::tempdir().unwrap();
        let gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let store = store_in(dir.path());
        store.save_snapshot(&gb, Some(b"a"), 10).unwrap();
        store.save_snapshot(&gb, Some(b"b"), 20).unwrap();
        store.delete_snapshot(10);
        store.delete_snapshot(10); // already gone: fine
        let left: Vec<u64> = store.snapshots().iter().map(|s| s.secs).collect();
        assert_eq!(left, [20]);
        assert!(!store.paths.snapshot_image(10).exists());
        assert!(store.paths.snapshot_image(20).exists());
    }

    #[test]
    fn the_cap_removes_the_oldest_snapshot_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let store = store_in(dir.path());
        for n in 1..=MAX_SNAPSHOTS as u64 {
            let saved = store.save_snapshot(&gb, Some(b"p"), n * 10).unwrap();
            assert_eq!(saved.evicted, 0, "snapshot {n}");
        }
        assert_eq!(store.snapshots().len(), MAX_SNAPSHOTS);
        let saved = store.save_snapshot(&gb, Some(b"p"), 10_000).unwrap();
        assert_eq!(
            saved,
            SnapshotSaved {
                secs: 10_000,
                evicted: 1
            }
        );
        let list = store.snapshots();
        assert_eq!(list.len(), MAX_SNAPSHOTS);
        assert_eq!(list.first().unwrap().secs, 10_000);
        assert_eq!(list.last().unwrap().secs, 20, "the snapshot at 10 went");
        assert!(!store.paths.snapshot_state(10).exists());
        assert!(!store.paths.snapshot_image(10).exists(), "its picture too");
    }

    #[test]
    fn a_model_mismatch_names_the_model_flag() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_in(dir.path());
        let dmg = GameBoy::with_model(cgb_rom(0x02), Model::Dmg).unwrap();
        let mut cgb = GameBoy::with_model(cgb_rom(0x02), Model::Cgb).unwrap();
        store.save_snapshot(&dmg, None, 1).unwrap(); // a DMG state, as every pre-CGB state is
        let err = store.load_snapshot(&mut cgb, 1).unwrap_err();
        assert!(err.contains("different hardware model"), "{err}");
        assert!(err.contains("--model dmg"), "{err}");

        store.save_snapshot(&cgb, None, 2).unwrap();
        let mut dmg = dmg;
        assert!(
            store
                .load_snapshot(&mut dmg, 2)
                .unwrap_err()
                .contains("--model cgb")
        );
    }

    #[test]
    fn failed_snapshot_loads_leave_the_machine_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let before = gb.save_state();
        let mut store = store_in(dir.path());
        assert!(
            store
                .load_snapshot(&mut gb, 1)
                .unwrap_err()
                .contains("gone")
        );
        let paths = store.paths.clone();
        let put = |data: &[u8]| {
            fs::create_dir_all(paths.states_dir()).unwrap();
            fs::write(paths.snapshot_state(1), data).unwrap();
        };
        put(b"not a state");
        assert!(
            store
                .load_snapshot(&mut gb, 1)
                .unwrap_err()
                .contains("not a save state")
        );
        put(&GameBoy::new(battery_rom(0x03)).unwrap().save_state());
        assert!(
            store
                .load_snapshot(&mut gb, 1)
                .unwrap_err()
                .contains("different ROM")
        );
        put(&vec![0; GB_MAX_STATE_BYTES as usize + 1]);
        assert!(
            store
                .load_snapshot(&mut gb, 1)
                .unwrap_err()
                .contains("larger than")
        );
        assert_eq!(gb.save_state(), before);
    }

    #[test]
    fn the_first_snapshot_load_backs_up_the_battery_save_once() {
        let dir = tempfile::tempdir().unwrap();
        let sav = dir.path().join("g.sav");
        let mut gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let mut store = store_in(dir.path());
        store.save_snapshot(&gb, None, 1).unwrap();
        fs::write(&sav, vec![0x42; 0x2000]).unwrap();

        let backup = store.load_snapshot(&mut gb, 1).unwrap().unwrap();
        assert!(backup.ends_with("g.sav.bak"), "{}", backup.display());
        assert_eq!(fs::read(&backup).unwrap(), vec![0x42; 0x2000]);

        fs::write(&sav, vec![0x43; 0x2000]).unwrap();
        assert_eq!(store.load_snapshot(&mut gb, 1), Ok(None));
        assert!(
            !dir.path().join("g.sav.bak1").exists(),
            "only once per session"
        );
    }

    #[test]
    fn a_failed_load_still_backs_up_before_the_next_try() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("g.sav"), vec![1; 0x2000]).unwrap();
        let mut gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let mut store = store_in(dir.path());
        fs::create_dir_all(store.paths.states_dir()).unwrap();
        fs::write(store.paths.snapshot_state(1), b"junk").unwrap();
        assert!(store.load_snapshot(&mut gb, 1).is_err());
        assert!(!store.sav_backed_up);
    }

    #[test]
    fn a_resume_point_round_trips_and_a_missing_one_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let mut gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let mut store = store_in(dir.path());
        assert_eq!(store.load_resume(&mut gb), Resume::Absent);
        gb.step_frame();
        let left = gb.save_state();
        store.save_resume(&gb).unwrap();
        assert!(
            store
                .paths
                .resume_state()
                .ends_with("g.states/resume.state")
        );
        for _ in 0..10 {
            gb.step_frame();
        }
        assert_eq!(store.load_resume(&mut gb), Resume::Loaded { backup: None });
        assert_eq!(gb.save_state(), left);
        store.discard_resume();
        store.discard_resume();
        assert_eq!(store.load_resume(&mut gb), Resume::Absent);
    }

    #[test]
    fn an_unusable_resume_point_is_kept_as_bad_and_the_machine_is_left_alone() {
        let other = GameBoy::new(battery_rom(0x03)).unwrap().save_state();
        let bad_files: [Vec<u8>; 3] = [
            b"not a state".to_vec(),
            other,
            vec![0; GB_MAX_STATE_BYTES as usize + 1],
        ];
        for content in bad_files {
            let dir = tempfile::tempdir().unwrap();
            let mut gb = GameBoy::new(battery_rom(0x02)).unwrap();
            let before = gb.save_state();
            let mut store = store_in(dir.path());
            fs::create_dir_all(store.paths.states_dir()).unwrap();
            fs::write(store.paths.resume_state(), &content).unwrap();

            let Resume::Failed { why, kept } = store.load_resume(&mut gb) else {
                panic!("a bad file must not load");
            };
            assert!(!why.is_empty());
            let kept = kept.expect("kept aside");
            assert!(kept.ends_with("resume.state.bad"));
            assert_eq!(
                fs::read(&kept).unwrap(),
                content,
                "not deleted, not changed"
            );
            assert!(!store.paths.resume_state().exists());
            assert_eq!(gb.save_state(), before);
            assert_eq!(
                store.load_resume(&mut gb),
                Resume::Absent,
                "only tried once"
            );
        }
    }

    #[test]
    fn a_resume_point_older_than_the_battery_save_backs_it_up_first() {
        let dir = tempfile::tempdir().unwrap();
        let sav = dir.path().join("g.sav");
        let mut gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let mut store = store_in(dir.path());
        store.save_resume(&gb).unwrap();
        fs::write(&sav, vec![0x42; 0x2000]).unwrap();

        // The usual case: the save was written first, then the resume point.
        set_mtime(&sav, 1_000);
        set_mtime(&store.paths.resume_state(), 2_000);
        assert_eq!(store.load_resume(&mut gb), Resume::Loaded { backup: None });
        assert!(!dir.path().join("g.sav.bak").exists());

        // After a crash the game's own save is the newer one.
        set_mtime(&sav, 3_000);
        let Resume::Loaded {
            backup: Some(backup),
        } = store.load_resume(&mut gb)
        else {
            panic!("the older resume point must back the save up");
        };
        assert_eq!(fs::read(backup).unwrap(), vec![0x42; 0x2000]);
    }

    #[test]
    fn old_slots_in_the_app_folder_become_snapshots_with_their_own_times() {
        let dir = tempfile::tempdir().unwrap();
        let data = crate::paths::DataDir::at(dir.path().join("data"))
            .created()
            .unwrap();
        let paths = GamePaths::new(&data, &crate::library::GameId::new("MOTH", 7));
        fs::write(paths.legacy_state(1), b"one").unwrap();
        fs::write(paths.legacy_state(9), b"nine").unwrap();
        fs::write(paths.legacy_state(4), b"same time as one").unwrap();
        set_mtime(&paths.legacy_state(1), 5_000);
        set_mtime(&paths.legacy_state(4), 5_000);
        set_mtime(&paths.legacy_state(9), 9_000);

        let rom = dir.path().join("moth.gb");
        assert_eq!(migrate_legacy_states(&rom, &paths), 3);
        assert_eq!(fs::read(paths.snapshot_state(5_000)).unwrap(), b"one");
        assert_eq!(
            fs::read(paths.snapshot_state(5_001)).unwrap(),
            b"same time as one"
        );
        assert_eq!(fs::read(paths.snapshot_state(9_000)).unwrap(), b"nine");
        assert!(!paths.snapshot_image(5_000).exists(), "no picture");
        assert!(!paths.legacy_state(1).exists(), "moved, not copied");
        assert_eq!(migrate_legacy_states(&rom, &paths), 0, "nothing left to do");
        assert_eq!(StateStore::new(paths).snapshots().len(), 3);
    }

    #[test]
    fn slots_beside_the_rom_are_converted_once_and_left_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let data = crate::paths::DataDir::at(dir.path().join("data"))
            .created()
            .unwrap();
        let paths = GamePaths::new(&data, &crate::library::GameId::new("MOTH", 7));
        let rom = dir.path().join("moth.gb");
        fs::write(state_path(&rom, 2), b"two").unwrap();
        set_mtime(&state_path(&rom, 2), 4_000);
        fs::write(state_path(&rom, 3), vec![0; 3 * 1024 * 1024]).unwrap(); // too big

        assert_eq!(migrate_legacy_states(&rom, &paths), 1);
        assert_eq!(fs::read(paths.snapshot_state(4_000)).unwrap(), b"two");
        assert!(state_path(&rom, 2).exists(), "the original stays");

        // Opening it again must not pile up copies, even after the snapshot is deleted.
        assert_eq!(migrate_legacy_states(&rom, &paths), 0);
        StateStore::new(paths.clone()).delete_snapshot(4_000);
        assert_eq!(migrate_legacy_states(&rom, &paths), 0);
        assert!(StateStore::new(paths).snapshots().is_empty());
    }

    #[test]
    fn with_no_app_folder_the_slots_beside_the_rom_are_converted_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let rom = dir.path().join("g.gb");
        let paths = GamePaths::beside(&rom);
        fs::write(dir.path().join("g.ss1"), b"one").unwrap();
        set_mtime(&dir.path().join("g.ss1"), 4_000);
        assert_eq!(migrate_legacy_states(&rom, &paths), 1);
        assert_eq!(
            fs::read(dir.path().join("g.states/snap-4000.state")).unwrap(),
            b"one"
        );
        assert!(dir.path().join("g.ss1").exists());
        assert_eq!(migrate_legacy_states(&rom, &paths), 0);
    }

    #[test]
    fn converting_more_slots_than_fit_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let gb = GameBoy::new(battery_rom(0x02)).unwrap();
        let data = crate::paths::DataDir::at(dir.path().join("data"))
            .created()
            .unwrap();
        let paths = GamePaths::new(&data, &crate::library::GameId::new("G", 1));
        let store = StateStore::new(paths.clone());
        for n in 1..=MAX_SNAPSHOTS as u64 {
            store.save_snapshot(&gb, None, 100_000 + n).unwrap();
        }
        // A slot of the app folder is converted even though the folder exists, then the
        // oldest snapshot goes.
        fs::write(paths.legacy_state(1), b"old").unwrap();
        set_mtime(&paths.legacy_state(1), 10);
        // A slot beside the ROM is not, now that the folder exists.
        let rom = dir.path().join("g.gb");
        fs::write(state_path(&rom, 2), b"beside").unwrap();
        assert_eq!(migrate_legacy_states(&rom, &paths), 1);
        let list = store.snapshots();
        assert_eq!(list.len(), MAX_SNAPSHOTS);
        assert!(list.iter().all(|s| s.secs != 10), "the oldest went");
    }
}
