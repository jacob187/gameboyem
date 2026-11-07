//! The games that ship inside the app (Snake, Breakout, cgbdemo, GRAINS, MOTH) and
//! where to find them at run time.
//!
//! The release workflow puts the ROMs in a `games` folder that the packager installs
//! as app resources. [`bundled_games_dir`] finds that folder from the running
//! executable; [`register_all`] lists its ROMs in the library, in place (nothing is
//! copied; saves still go to the app folder).
//!
//! Where `games` is, relative to the executable `exe` (first match that is a folder):
//!
//! | Package | Executable | Games |
//! |---------|------------|-------|
//! | AppImage | `$APPDIR/usr/bin/gb` | `$APPDIR/usr/lib/gb/games` |
//! | macOS `.app` | `X.app/Contents/MacOS/gb` | `X.app/Contents/Resources/games` |
//! | `.deb` | `/usr/bin/gb` | `/usr/lib/gb/games` (the packager uses `usr/lib/<binary name>`) |
//! | Windows installer, portable zip, Linux `.tar.gz` | `<dir>/gb[.exe]` | `<dir>/games` |
//!
//! `../lib/gameboyem/games` and `../share/gameboyem/games` are tried as well, for a
//! package built by other means.
//!
//! A development build (`cargo run`, running from `target/`) has no such folder and
//! bundles nothing: it does not fall back to `roms/homebrew`. To try the games there,
//! build them (`make -C homebrew`, `make -C homebrew gba`) and set
//! `GAMEBOYEM_BUNDLED_DIR=roms/homebrew`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::library::{BundledReport, Library};

/// Environment variable that names the bundled games folder directly (development,
/// tests). It wins over the layouts above and is used as given, even if it is not a
/// folder, so a wrong value shows up as a warning rather than silently bundling nothing.
pub const ENV_BUNDLED_DIR: &str = "GAMEBOYEM_BUNDLED_DIR";

/// Name of the folder inside the package.
const GAMES: &str = "games";
/// Folder name under `lib/` and `share/` for packages not made by the packager.
const APP_DIR: &str = "gameboyem";

/// The folder holding the bundled games for this process, or `None` (a development
/// build, or a package without them).
pub fn bundled_games_dir() -> Option<PathBuf> {
    resolve(
        std::env::current_exe().ok().as_deref(),
        std::env::var_os(ENV_BUNDLED_DIR),
        std::env::var_os("APPDIR"),
    )
}

/// [`bundled_games_dir`] over its inputs: the executable's path, the value of
/// `GAMEBOYEM_BUNDLED_DIR` and the value of `APPDIR`. The same candidates are tried on
/// every OS (only the ones that exist as folders match), so each layout is testable
/// anywhere.
fn resolve(
    exe: Option<&Path>,
    override_dir: Option<OsString>,
    appdir: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(dir) = override_dir.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let exe = exe?;
    let exe_name = exe.file_name()?;
    let exe_dir = exe.parent()?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    // Inside a mounted AppImage. A stale APPDIR inherited from another AppImage is
    // ignored: the executable must really be under it.
    if let Some(appdir) = appdir.filter(|v| !v.is_empty()).map(PathBuf::from)
        && exe.starts_with(&appdir)
    {
        candidates.push(appdir.join("usr/lib").join(exe_name).join(GAMES));
        candidates.push(appdir.join("usr/lib").join(APP_DIR).join(GAMES));
        candidates.push(appdir.join("usr/share").join(APP_DIR).join(GAMES));
    }
    // `exe_dir` is `<root>/bin` or `Contents/MacOS`; its parent is the package root.
    // The paths are built without `..` so the library index holds clean paths.
    if let Some(root) = exe_dir.parent() {
        candidates.push(root.join("Resources").join(GAMES));
        candidates.push(root.join("lib").join(exe_name).join(GAMES));
        candidates.push(root.join("lib").join(APP_DIR).join(GAMES));
        candidates.push(root.join("share").join(APP_DIR).join(GAMES));
    }
    // Last, so an unrelated `games` folder next to a `gb` on the PATH is the least likely match.
    candidates.push(exe_dir.join(GAMES));
    candidates.into_iter().find(|dir| dir.is_dir())
}

/// List the bundled games in `lib`, if this process has a bundled games folder, and
/// log what changed. Safe at every start. A failure is logged as a warning and
/// returns `None`; startup never depends on it.
pub fn register_all(lib: &mut Library) -> Option<BundledReport> {
    register_from(lib, bundled_games_dir()?)
}

fn register_from(lib: &mut Library, dir: PathBuf) -> Option<BundledReport> {
    match lib.register_bundled(&dir) {
        Ok(report) => {
            log::debug!(
                "bundled games in {}: {} added, {} moved, {} removed, {} skipped",
                dir.display(),
                report.added,
                report.updated,
                report.removed,
                report.skipped
            );
            Some(report)
        }
        Err(e) => {
            log::warn!("{e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::library::Source;
    use crate::paths::DataDir;

    fn gb_rom(title: &str) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x134..0x134 + title.len()].copy_from_slice(title.as_bytes());
        rom
    }

    fn gba_rom(title: &str) -> Vec<u8> {
        let mut rom = vec![0u8; 0x200];
        rom[..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
        rom[0xA0..0xA0 + title.len()].copy_from_slice(title.as_bytes());
        rom
    }

    fn mkdir(path: &Path) -> PathBuf {
        fs::create_dir_all(path).unwrap();
        path.to_path_buf()
    }

    fn os(s: impl Into<OsString>) -> Option<OsString> {
        Some(s.into())
    }

    #[test]
    fn macos_bundle_resources_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("gameboyem.app/Contents");
        let games = mkdir(&app.join("Resources/games"));
        mkdir(&app.join("MacOS"));
        let exe = app.join("MacOS/gb");
        assert_eq!(resolve(Some(&exe), None, None), Some(games));
    }

    #[test]
    fn windows_installer_and_archives_use_games_next_to_the_exe() {
        let tmp = tempfile::tempdir().unwrap();
        let games = mkdir(&tmp.path().join("gameboyem-0.1.0-linux/games"));
        for name in ["gb", "gb.exe"] {
            let exe = tmp.path().join("gameboyem-0.1.0-linux").join(name);
            assert_eq!(resolve(Some(&exe), None, None), Some(games.clone()));
        }
    }

    #[test]
    fn deb_uses_lib_named_after_the_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let games = mkdir(&tmp.path().join("usr/lib/gb/games"));
        mkdir(&tmp.path().join("usr/bin"));
        let exe = tmp.path().join("usr/bin/gb");
        assert_eq!(resolve(Some(&exe), None, None), Some(games));
    }

    #[test]
    fn other_linux_layouts_lib_and_share() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("a");
        let games = mkdir(&lib.join("lib/gameboyem/games"));
        assert_eq!(resolve(Some(&lib.join("bin/gb")), None, None), Some(games));

        let share = tmp.path().join("b");
        let games = mkdir(&share.join("share/gameboyem/games"));
        assert_eq!(
            resolve(Some(&share.join("bin/gb")), None, None),
            Some(games)
        );
    }

    #[test]
    fn appimage_uses_appdir() {
        let tmp = tempfile::tempdir().unwrap();
        let appdir = tmp.path().join("tmp/.mount_gbXYZ");
        let games = mkdir(&appdir.join("usr/lib/gb/games"));
        let exe = appdir.join("usr/bin/gb");
        assert_eq!(
            resolve(Some(&exe), None, os(appdir.as_os_str())),
            Some(games.clone())
        );
        // Without APPDIR the relative layout finds the same folder.
        assert_eq!(resolve(Some(&exe), None, None), Some(games));
    }

    #[test]
    fn a_stale_appdir_from_elsewhere_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let other = tmp.path().join("other.AppDir");
        mkdir(&other.join("usr/lib/gb/games"));
        let exe = tmp.path().join("install/gb");
        let games = mkdir(&tmp.path().join("install/games"));
        assert_eq!(
            resolve(Some(&exe), None, os(other.as_os_str())),
            Some(games)
        );
    }

    #[test]
    fn the_override_wins_and_is_used_as_given() {
        let tmp = tempfile::tempdir().unwrap();
        mkdir(&tmp.path().join("pkg/games"));
        let exe = tmp.path().join("pkg/gb");
        let custom = tmp.path().join("roms/homebrew");
        assert_eq!(
            resolve(Some(&exe), os(custom.as_os_str()), None),
            Some(custom.clone()),
            "used even though it does not exist"
        );
        mkdir(&custom);
        assert_eq!(
            resolve(Some(&exe), os(custom.as_os_str()), None),
            Some(custom)
        );
        assert_eq!(resolve(None, os("/x"), None), Some(PathBuf::from("/x")));
    }

    #[test]
    fn an_empty_override_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let games = mkdir(&tmp.path().join("pkg/games"));
        let exe = tmp.path().join("pkg/gb");
        assert_eq!(resolve(Some(&exe), os(""), None), Some(games));
    }

    #[test]
    fn a_development_build_has_no_bundled_folder() {
        // target/release/gb with no `games` anywhere near it (and none under
        // roms/homebrew: there is no fallback to it).
        let tmp = tempfile::tempdir().unwrap();
        mkdir(&tmp.path().join("roms/homebrew"));
        mkdir(&tmp.path().join("target/release"));
        let exe = tmp.path().join("target/release/gb");
        assert_eq!(resolve(Some(&exe), None, None), None);
        assert_eq!(resolve(None, None, None), None);
        assert_eq!(resolve(Some(Path::new("gb")), None, None), None);
    }

    #[test]
    fn a_file_named_games_is_not_a_folder() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("games"), b"x").unwrap();
        assert_eq!(resolve(Some(&tmp.path().join("gb")), None, None), None);
    }

    fn library(tmp: &Path) -> Library {
        let data = DataDir::at(tmp.join("app"));
        data.create().unwrap();
        Library::open(&data)
    }

    #[test]
    fn registering_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let games = mkdir(&tmp.path().join("pkg/games"));
        fs::write(games.join("one.gb"), gb_rom("ONE")).unwrap();
        fs::write(games.join("two.gba"), gba_rom("TWO")).unwrap();
        fs::write(games.join("LICENSE.txt"), b"MIT").unwrap();
        let mut lib = library(tmp.path());

        let first = register_from(&mut lib, games.clone()).unwrap();
        assert_eq!((first.added, first.skipped), (2, 0));
        assert_eq!(lib.entries().len(), 2);
        assert!(lib.entries().iter().all(|e| e.source == Source::Bundled));
        let snapshot = lib.entries().to_vec();

        let again = register_from(&mut lib, games).unwrap();
        assert_eq!(again, BundledReport::default());
        assert_eq!(lib.entries(), snapshot);
    }

    #[test]
    fn a_missing_folder_is_a_warning_not_a_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lib = library(tmp.path());
        assert_eq!(register_from(&mut lib, tmp.path().join("no/such")), None);
        assert!(lib.entries().is_empty());
    }

    #[test]
    fn register_all_does_nothing_without_a_bundled_folder() {
        // The test binary runs from target/, which has no `games` folder; skip if the
        // developer has pointed the override somewhere.
        if std::env::var_os(ENV_BUNDLED_DIR).is_some() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let mut lib = library(tmp.path());
        assert_eq!(register_all(&mut lib), None);
        assert_eq!(register_all(&mut lib), None);
        assert!(lib.entries().is_empty());
    }
}
