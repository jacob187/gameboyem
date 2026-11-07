//! The per-user app data folder and the layout inside it.
//!
//! ```text
//! <data dir>/
//!   games/        ROM copies (`<id>.gb|gbc|gba`), thumbnails (`<id>.png`), `library.json`
//!   saves/        battery saves (`<id>.sav`)
//!   states/       per game: `<id>/resume.state`, `<id>/snap-<unix-secs>.state` and `.png`
//!   screenshots/  `<id>-fNNNNNNN.png`
//!   logs/         `gameboyem.log` and its rotated files
//! ```
//!
//! The folder is the one the OS keeps application data in (see [`DataDir::locate`]),
//! or the one named by `GAMEBOYEM_DATA_DIR`. Resolving never touches the disk; only
//! [`DataDir::create`] does.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Environment variable that replaces the OS data folder (tests, portable installs).
pub const ENV_DATA_DIR: &str = "GAMEBOYEM_DATA_DIR";

/// The app's folder name. No qualifier or organization is passed to `directories`, so
/// the folder is plain `gameboyem` on every OS.
const APPLICATION: &str = "gameboyem";

const GAMES: &str = "games";
const SAVES: &str = "saves";
const STATES: &str = "states";
const SCREENSHOTS: &str = "screenshots";
const LOGS: &str = "logs";

/// File name of the library index inside `games/`.
pub const INDEX_FILE: &str = "library.json";
/// File name of the current log inside `logs/`.
pub const LOG_FILE: &str = "gameboyem.log";

/// The app data folder. Cheap to clone; holds only the root path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// A data folder rooted at `root`, as is.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The data folder for this process: `GAMEBOYEM_DATA_DIR` if set and not
    /// empty, else the OS location (macOS `~/Library/Application Support/gameboyem`,
    /// Windows `%APPDATA%\gameboyem\data`, Linux `$XDG_DATA_HOME/gameboyem` or
    /// `~/.local/share/gameboyem`).
    pub fn locate() -> Result<Self, String> {
        let os_default =
            directories::ProjectDirs::from("", "", APPLICATION).map(|d| d.data_dir().to_path_buf());
        Self::resolve(std::env::var_os(ENV_DATA_DIR), os_default)
    }

    /// Pick the folder from the override and the OS default. A relative override
    /// is made absolute against the current directory so a later `chdir` cannot
    /// move the data.
    pub fn resolve(
        override_dir: Option<OsString>,
        os_default: Option<PathBuf>,
    ) -> Result<Self, String> {
        match override_dir.filter(|v| !v.is_empty()) {
            Some(dir) => std::path::absolute(&dir).map(Self::at).map_err(|e| {
                format!(
                    "{ENV_DATA_DIR} is set to {}, which is not a usable path: {e}",
                    Path::new(&dir).display()
                )
            }),
            None => os_default.map(Self::at).ok_or_else(|| {
                format!(
                    "cannot tell where to keep game data (no home folder); set {ENV_DATA_DIR} to a folder"
                )
            }),
        }
    }

    /// [`DataDir::locate`] and [`DataDir::create`] in one step: the folder, ready to
    /// use, or why it cannot be. The caller carries on without it (saves go next to
    /// the ROM) rather than stopping.
    pub fn prepare() -> Result<Self, String> {
        Self::locate()?.created()
    }

    /// `self` after making the folders, or the reason that failed.
    pub fn created(self) -> Result<Self, String> {
        match self.create() {
            Ok(()) => Ok(self),
            Err(e) => Err(format!(
                "cannot create the data folder {}: {e}",
                self.root.display()
            )),
        }
    }

    /// Make the folder and every subfolder. Existing ones are left alone.
    pub fn create(&self) -> io::Result<()> {
        for dir in [
            self.games(),
            self.saves(),
            self.states(),
            self.screenshots(),
            self.logs(),
        ] {
            fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn games(&self) -> PathBuf {
        self.root.join(GAMES)
    }

    pub fn saves(&self) -> PathBuf {
        self.root.join(SAVES)
    }

    pub fn states(&self) -> PathBuf {
        self.root.join(STATES)
    }

    pub fn screenshots(&self) -> PathBuf {
        self.root.join(SCREENSHOTS)
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join(LOGS)
    }

    /// The library index.
    pub fn index_file(&self) -> PathBuf {
        self.games().join(INDEX_FILE)
    }

    /// The current log file.
    pub fn log_file(&self) -> PathBuf {
        self.logs().join(LOG_FILE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_override_wins_over_the_os_default() {
        let d = DataDir::resolve(
            Some("/data/gb".into()),
            Some(PathBuf::from("/home/u/.local/share/gameboyem")),
        )
        .unwrap();
        assert_eq!(d.root(), Path::new("/data/gb"));
    }

    #[test]
    fn an_empty_override_is_ignored() {
        let d = DataDir::resolve(Some("".into()), Some(PathBuf::from("/os/default"))).unwrap();
        assert_eq!(d.root(), Path::new("/os/default"));
    }

    #[test]
    fn a_relative_override_becomes_absolute() {
        let d = DataDir::resolve(Some("rel/dir".into()), None).unwrap();
        assert!(d.root().is_absolute());
        assert!(d.root().ends_with("rel/dir"));
    }

    #[test]
    fn no_override_and_no_home_is_an_error_naming_the_variable() {
        let e = DataDir::resolve(None, None).unwrap_err();
        assert!(e.contains(ENV_DATA_DIR), "{e}");
    }

    #[test]
    fn the_layout_is_fixed() {
        let d = DataDir::at("/r");
        assert_eq!(d.games(), Path::new("/r/games"));
        assert_eq!(d.saves(), Path::new("/r/saves"));
        assert_eq!(d.states(), Path::new("/r/states"));
        assert_eq!(d.screenshots(), Path::new("/r/screenshots"));
        assert_eq!(d.logs(), Path::new("/r/logs"));
        assert_eq!(d.index_file(), Path::new("/r/games/library.json"));
        assert_eq!(d.log_file(), Path::new("/r/logs/gameboyem.log"));
    }

    #[test]
    fn create_makes_every_folder_and_is_repeatable() {
        let tmp = tempfile::tempdir().unwrap();
        let d = DataDir::at(tmp.path().join("new/app"));
        d.create().unwrap();
        d.create().unwrap();
        for dir in [d.games(), d.saves(), d.states(), d.screenshots(), d.logs()] {
            assert!(dir.is_dir(), "{}", dir.display());
        }
    }

    #[test]
    fn a_folder_that_cannot_be_created_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("in-the-way");
        fs::write(&file, b"x").unwrap();
        let e = DataDir::at(file.join("app")).created().unwrap_err();
        assert!(e.contains("cannot create the data folder"), "{e}");
        assert!(DataDir::at(tmp.path().join("ok")).created().is_ok());
    }

    #[test]
    fn the_os_default_is_inside_a_gameboyem_folder() {
        // Whatever the platform, the OS location must name the app (or be absent
        // on a machine with no home folder).
        let d = directories::ProjectDirs::from("", "", APPLICATION);
        if let Some(d) = d {
            let path = d.data_dir().to_string_lossy().into_owned();
            assert!(path.contains("gameboyem"), "{path}");
            // No organization or qualifier: the folder carries no account or domain name.
            assert!(!path.contains("io.github"), "{path}");
        }
    }
}
