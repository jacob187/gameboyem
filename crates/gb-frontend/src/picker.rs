//! Choosing ROMs with the native file dialog, and what to do when `gb` is started without
//! one, as it is from Finder, the Start menu or a desktop launcher: show the library.

use std::path::PathBuf;

/// The ROM file extensions the open dialog offers, lowercase and without the dot.
/// Archives are not listed: the frontend does not unpack them (see `rom.rs`).
pub const ROM_EXTENSIONS: [&str; 3] = ["gb", "gbc", "gba"];

/// The name of the dialog's file filter.
const FILTER_NAME: &str = "Game Boy and Game Boy Advance ROMs";

/// What to do about the ROM at startup.
#[derive(Debug, PartialEq, Eq)]
pub enum Startup {
    /// A path was given: use it, with no dialog.
    Given(PathBuf),
    /// No path and an interactive run: show the game library.
    Library,
    /// No path in a mode that cannot ask (`--test-pattern`): a usage error.
    Missing,
}

/// Decide how to get the ROM. `--test-pattern` has no library to show, so a
/// scripted run does not wait for a click.
pub fn startup(rom: Option<PathBuf>, test_pattern: bool) -> Startup {
    match rom {
        Some(path) => Startup::Given(path),
        None if test_pattern => Startup::Missing,
        None => Startup::Library,
    }
}

/// Show the native open dialog for any number of ROMs. Empty when it is cancelled (or no
/// dialog backend is available; the library cannot tell the two apart).
pub fn pick_roms() -> Vec<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Add games")
        .add_filter(FILTER_NAME, &ROM_EXTENSIONS)
        .pick_files()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_given_path_is_used_without_asking() {
        for test_pattern in [false, true] {
            assert_eq!(
                startup(Some("g.gb".into()), test_pattern),
                Startup::Given("g.gb".into())
            );
        }
    }

    #[test]
    fn no_path_shows_the_library_unless_the_mode_cannot() {
        assert_eq!(startup(None, false), Startup::Library);
        assert_eq!(startup(None, true), Startup::Missing);
    }

    #[test]
    fn the_filter_lists_the_game_boy_and_gba_extensions() {
        assert_eq!(ROM_EXTENSIONS, ["gb", "gbc", "gba"]);
        assert!(
            ROM_EXTENSIONS
                .iter()
                .all(|e| !e.starts_with('.') && e.chars().all(|c| c.is_ascii_lowercase()))
        );
    }
}
