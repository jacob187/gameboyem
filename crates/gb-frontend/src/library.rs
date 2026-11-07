//! The game library and the files that belong to a game.
//!
//! A game is known by its [`GameId`]: a readable slug from the ROM header title
//! and the FNV-1a hash of the ROM bytes (`moth-0123456789abcdef`). Everything the
//! player produces is keyed by that id, in the app folder (see `paths.rs`):
//! `saves/<id>.sav`, `states/<id>/resume.state` and `states/<id>/snap-<unix-secs>.state`
//! (with a matching `.png`), `screenshots/<id>-fNNNNNNN.png`.
//! Nothing depends on the ROM's file name or location, so renaming or moving a
//! ROM keeps its saves, and a ROM opened from the command line shares them with
//! the same game added to the library. (The slug comes from the header, never the
//! file name, for the same reason.)
//!
//! The library itself is `games/library.json` plus a copy of each added ROM in
//! `games/<id>.gb|gbc|gba` and an optional thumbnail `games/<id>.png`:
//!
//! ```json
//! {"version":1,"games":[{"id":"moth-0123456789abcdef","title":"MOTH","system":"gbc",
//!   "source":"imported","rom":"moth-0123456789abcdef.gbc","hash":"0123456789abcdef",
//!   "added":1767225600,"last_played":null}]}
//! ```
//!
//! Times are Unix seconds. `rom` is a file name in `games/` for an imported game
//! and an absolute path for a bundled one. The index is untrusted input: it is
//! size-capped, each entry is checked and a bad one is dropped, and an index that
//! cannot be used is copied aside and rebuilt from the ROMs in `games/`. The
//! files to delete are always derived from the id, never read from the index.

#![allow(dead_code, reason = "the library screen is built on this API next")]

use std::fmt;
use std::fs;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use emu_common::{Core, System, fnv1a};
use gb_core::{GameBoy, Model};
use gba_core::{CartOptions, Gba};
use serde::{Deserialize, Serialize};

use crate::paths::DataDir;
use crate::rom;
use crate::save::{MAX_SAVE_BYTES, append_ext, read_capped, save_path, write_atomic};
use crate::state;

/// Version of the index file format.
const INDEX_VERSION: u32 = 1;
/// A larger index is not read.
const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;
/// Entries read from an index; the rest are ignored.
const MAX_ENTRIES: usize = 10_000;
/// Longest title kept, in characters.
const MAX_TITLE_CHARS: usize = 64;
/// Longest readable prefix of an id.
const MAX_SLUG_CHARS: usize = 24;
/// The prefix of a game whose header has no usable title.
const FALLBACK_SLUG: &str = "game";
/// File name of the resume point inside a game's states folder.
pub(crate) const RESUME_FILE: &str = "resume.state";
/// Longest bundled ROM path read from the index.
const MAX_PATH_CHARS: usize = 4096;
/// Largest thumbnail PNG accepted, in bytes, and its largest side, in pixels.
const MAX_THUMBNAIL_BYTES: u64 = 1024 * 1024;
const MAX_THUMBNAIL_SIDE: u32 = 1024;

/// Extensions of the ROM copies in `games/` (the same list the open dialog uses).
const ROM_EXTENSIONS: [&str; 3] = ["gb", "gbc", "gba"];

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// A game's identity: `<slug>-<16 hex digits of the ROM hash>`. Only built from
/// ROM bytes ([`GameId::new`]) or checked text ([`GameId::parse`]), so it is
/// always a safe file name stem: lowercase ASCII letters, digits and `-`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GameId(String);

impl GameId {
    /// The id for a ROM whose header title is `title` and whose bytes hash to
    /// `hash` ([`rom_hash`]). Use the title the core reports ([`Core::title`]),
    /// so every way of opening the same ROM gives the same id.
    pub fn new(title: &str, hash: u64) -> Self {
        Self(format!("{}-{hash:016x}", slug(title)))
    }

    /// `text` as an id, if it has the shape [`GameId::new`] makes.
    pub fn parse(text: &str) -> Option<Self> {
        let (prefix, digits) = text.rsplit_once('-')?;
        let digits_ok = digits.len() == 16
            && digits
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let prefix_ok = !prefix.is_empty()
            && prefix.len() <= MAX_SLUG_CHARS
            && !prefix.starts_with('-')
            && !prefix.ends_with('-')
            && !prefix.contains("--")
            && prefix
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-');
        (digits_ok && prefix_ok).then(|| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The ROM hash the id ends with.
    pub fn hash(&self) -> u64 {
        // Checked to be 16 hex digits on construction.
        self.0
            .rsplit_once('-')
            .and_then(|(_, digits)| u64::from_str_radix(digits, 16).ok())
            .unwrap_or_default()
    }
}

impl fmt::Display for GameId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The hash games are keyed by: 64-bit FNV-1a of the ROM bytes, the one the
/// cores compute (`GameBoy::rom_hash`, `Gba::rom_hash`) and recordings store.
pub fn rom_hash(rom: &[u8]) -> u64 {
    fnv1a(rom)
}

/// Lowercase ASCII letters and digits of `title`, other runs joined by one `-`,
/// at most [`MAX_SLUG_CHARS`] long; [`FALLBACK_SLUG`] if nothing is left.
fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(MAX_SLUG_CHARS);
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        FALLBACK_SLUG.to_owned()
    } else {
        out.to_owned()
    }
}

/// Which console a game is for. `System` has no Game Boy Color: it is a Game Boy
/// whose header says it supports colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameSystem {
    Gb,
    Gbc,
    Gba,
}

impl GameSystem {
    /// The short name shown to people: `GB`, `GBC` or `GBA`.
    pub fn label(self) -> &'static str {
        match self {
            GameSystem::Gb => "GB",
            GameSystem::Gbc => "GBC",
            GameSystem::Gba => "GBA",
        }
    }

    /// The emulated machine.
    pub fn system(self) -> System {
        match self {
            GameSystem::Gb | GameSystem::Gbc => System::Gb,
            GameSystem::Gba => System::Gba,
        }
    }

    /// The extension of a ROM file of this system, and the name used in the index.
    fn extension(self) -> &'static str {
        match self {
            GameSystem::Gb => "gb",
            GameSystem::Gbc => "gbc",
            GameSystem::Gba => "gba",
        }
    }

    fn from_extension(text: &str) -> Option<Self> {
        match text {
            "gb" => Some(GameSystem::Gb),
            "gbc" => Some(GameSystem::Gbc),
            "gba" => Some(GameSystem::Gba),
            _ => None,
        }
    }
}

/// Where a game's ROM lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Added by the player: a copy in `games/`, which the library owns.
    Imported,
    /// Shipped with the app: the ROM stays where it is (read-only resources) and
    /// only the player's files go to the app folder.
    Bundled,
}

impl Source {
    fn name(self) -> &'static str {
        match self {
            Source::Imported => "imported",
            Source::Bundled => "bundled",
        }
    }

    fn from_name(text: &str) -> Option<Self> {
        match text {
            "imported" => Some(Source::Imported),
            "bundled" => Some(Source::Bundled),
            _ => None,
        }
    }
}

/// One game in the library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: GameId,
    /// The header title, or the file name it was added under if the header has none.
    pub title: String,
    pub system: GameSystem,
    pub source: Source,
    /// Where to read the ROM from: a copy in `games/` or the bundled file.
    pub rom_path: PathBuf,
    /// Unix seconds when the game was added.
    pub added: u64,
    /// Unix seconds when it was last played, `None` if never.
    pub last_played: Option<u64>,
    /// The thumbnail PNG, if one was set.
    pub thumbnail: Option<PathBuf>,
    /// The ROM file is there. Worked out when the library is read, not stored: a
    /// game whose file is missing for now (a drive that is not mounted, a bundled
    /// ROM of an app that moved) stays listed, with its saves, and cannot be played.
    pub available: bool,
    /// For a bundled game: the size and modification time of the ROM file when it was
    /// last read, so a start can skip reading a file that has not changed.
    pub(crate) stamp: Option<RomStamp>,
}

/// A cheap fingerprint of a file: if it is the same as last time, so is the content
/// (for the purpose of not reading the ROM again). Any value is harmless in the
/// index: a stamp that does not match just means the file is read again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RomStamp {
    size: u64,
    mtime_ns: u64,
}

impl RomStamp {
    fn of(path: &Path) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        let since_epoch = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(Self {
            size: meta.len(),
            mtime_ns: u64::try_from(since_epoch.as_nanos()).ok()?,
        })
    }
}

// ---------------------------------------------------------------------------
// Files that belong to a game
// ---------------------------------------------------------------------------

/// The player's files for one game.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GamePaths {
    /// The battery save.
    pub sav: PathBuf,
    /// The folder of the resume point and the snapshots: `states/<id>/`.
    states_dir: PathBuf,
    /// Where older versions kept the numbered slots (`<prefix>.ss1`..`.ss9`); read only to
    /// convert them to snapshots ([`state::migrate_legacy_states`]).
    legacy_prefix: PathBuf,
    screenshot_prefix: PathBuf,
}

impl GamePaths {
    /// The files of game `id` in `data`.
    pub fn new(data: &DataDir, id: &GameId) -> Self {
        let name = id.as_str();
        Self {
            sav: data.saves().join(format!("{name}.sav")),
            states_dir: data.states().join(name),
            legacy_prefix: data.states().join(name),
            screenshot_prefix: data.screenshots().join(name),
        }
    }

    /// The folder holding this game's resume point and snapshots. It is made when the
    /// first file is written; its existence also means the old slots were dealt with.
    pub fn states_dir(&self) -> &Path {
        &self.states_dir
    }

    /// Where the game is left when the player leaves it: `states/<id>/resume.state`.
    pub fn resume_state(&self) -> PathBuf {
        self.states_dir.join(RESUME_FILE)
    }

    /// Where a resume point that could not be loaded is kept.
    pub fn resume_bad(&self) -> PathBuf {
        self.states_dir.join(format!("{RESUME_FILE}.bad"))
    }

    /// The snapshot taken at `unix_secs`: `states/<id>/snap-<unix-secs>.state`.
    pub fn snapshot_state(&self, unix_secs: u64) -> PathBuf {
        self.states_dir.join(format!("snap-{unix_secs}.state"))
    }

    /// The picture of that snapshot: `states/<id>/snap-<unix-secs>.png`.
    pub fn snapshot_image(&self, unix_secs: u64) -> PathBuf {
        self.states_dir.join(format!("snap-{unix_secs}.png"))
    }

    /// The numbered slot `slot` (1..=9) of older versions, `states/<id>.ss<slot>`.
    pub(crate) fn legacy_state(&self, slot: u8) -> PathBuf {
        append_ext(&self.legacy_prefix, &format!("ss{slot}"))
    }

    /// The screenshot of emulated frame `frame` taken at `unix_secs` (wall clock, UTC):
    /// `screenshots/<id>-20261107-190305-f0001234.png`. The time keeps a later session's
    /// screenshot of the same frame number from replacing this one.
    pub fn screenshot(&self, unix_secs: u64, frame: u64) -> PathBuf {
        let mut name = self.screenshot_prefix.clone().into_os_string();
        name.push(format!(
            "-{}-f{frame:07}.png",
            crate::logging::utc_compact(unix_secs)
        ));
        PathBuf::from(name)
    }

    /// The layout for when there is no app folder: `dir/game.sav`, `dir/game.states/`
    /// and `dir/game-<time>-f0000001.png` for the ROM `dir/game.gb` (and, where older
    /// versions kept them, `dir/game.ss1`..`.ss9`). A ROM named like one of those files
    /// keeps its whole name as the stem, so a save can never be the ROM itself.
    pub fn beside(rom: &Path) -> Self {
        let named_like_a_slot = rom.extension().is_some_and(|e| {
            let e = e.to_string_lossy().to_ascii_lowercase();
            matches!(e.as_bytes(), [b's', b's', b'1'..=b'9'])
        });
        let stem = if named_like_a_slot {
            rom.to_owned()
        } else {
            rom.with_extension("")
        };
        let states_dir = append_ext(&stem, "states");
        Self {
            sav: save_path(rom),
            states_dir: if states_dir == rom {
                append_ext(rom, "states")
            } else {
                states_dir
            },
            legacy_prefix: stem.clone(),
            screenshot_prefix: stem,
        }
    }
}

/// The files for a ROM opened directly (a path on the command line, a drop on
/// the window), which is not added to the library: its [`GamePaths`], after
/// copying in any `.sav` that sits next to the ROM and turning any old `.ss1`..`.ss9`
/// into snapshots ([`import_beside_rom`]). `title` and `hash` are the core's. With no app folder
/// (`data` is `None`) the files stay next to the ROM, as before; only the old slots are
/// turned into snapshots (in a folder beside the ROM).
pub fn paths_for_opened_rom(
    data: Option<&DataDir>,
    rom_path: &Path,
    title: &str,
    hash: u64,
) -> GamePaths {
    let Some(data) = data else {
        let paths = GamePaths::beside(rom_path);
        state::migrate_legacy_states(rom_path, &paths);
        return paths;
    };
    let paths = GamePaths::new(data, &GameId::new(title, hash));
    import_beside_rom(rom_path, &paths);
    paths
}

/// Copy the save that sits next to `rom` (`game.sav`, where older versions kept it)
/// into the app folder if there is none there yet, and turn the old save-state slots
/// (`game.ss1`..`game.ss9` beside the ROM, `states/<id>.ssN` in the app folder) into
/// snapshots ([`state::migrate_legacy_states`]). The originals beside the ROM are never
/// changed. A file already in the app folder always wins over the one beside the ROM,
/// even if that one is newer: progress made here is never overwritten, and the atomic
/// writes make a half-written app copy unlikely.
/// A file that is oversized or unreadable is skipped with a warning. Returns
/// the saves copied, as (from, to).
pub fn import_beside_rom(rom: &Path, paths: &GamePaths) -> Vec<(PathBuf, PathBuf)> {
    state::migrate_legacy_states(rom, paths);
    let wanted = [(save_path(rom), paths.sav.clone(), MAX_SAVE_BYTES, "save")];
    let mut copied = Vec::new();
    for (from, to, cap, what) in wanted {
        // `symlink_metadata`: a dangling link in the app folder still counts as "there".
        if from == to || fs::symlink_metadata(&to).is_ok() {
            continue;
        }
        let data = match read_capped(&from, cap, what) {
            Ok(Some(data)) => data,
            Ok(None) => continue,
            Err(e) => {
                log::warn!("not importing {what} {}: {e}", from.display());
                continue;
            }
        };
        let written = match to.parent() {
            Some(dir) => fs::create_dir_all(dir),
            None => Ok(()),
        }
        .and_then(|()| write_atomic(&to, &data));
        match written {
            Ok(()) => {
                log::info!(
                    "imported {what} {} to {} (the original is left as it was)",
                    from.display(),
                    to.display()
                );
                copied.push((from, to));
            }
            Err(e) => log::warn!(
                "cannot import {what} {} to {}: {e}",
                from.display(),
                to.display()
            ),
        }
    }
    copied
}

// ---------------------------------------------------------------------------
// Reading a ROM
// ---------------------------------------------------------------------------

/// A ROM file that passed the checks `gb` runs before powering on.
struct Probe {
    rom: Vec<u8>,
    game: GameSystem,
    /// The core's title, the one the id comes from.
    header_title: String,
    hash: u64,
}

impl Probe {
    /// Read `path` with the size caps and system detection of `rom.rs`, and make
    /// sure the matching core accepts it.
    fn read(path: &Path) -> Result<Self, String> {
        let (system, rom) = rom::load_rom(path, None)?;
        let archive = rom::archive_kind(&rom);
        let (game, header_title) = match system {
            System::Gb => {
                let gb = GameBoy::with_model(rom.clone(), Model::Dmg)
                    .map_err(|e| rom::cartridge_message(path, &e, archive))?;
                let game = if gb.cartridge_header().supports_cgb() {
                    GameSystem::Gbc
                } else {
                    GameSystem::Gb
                };
                (game, Core::title(&gb))
            }
            System::Gba => {
                let gba = Gba::with_options(rom.clone(), None, CartOptions::default())
                    .map_err(|e| rom::gba_cartridge_message(path, &e, archive))?;
                (GameSystem::Gba, Core::title(&gba))
            }
        };
        let hash = rom_hash(&rom);
        Ok(Self {
            rom,
            game,
            header_title,
            hash,
        })
    }

    fn id(&self) -> GameId {
        GameId::new(&self.header_title, self.hash)
    }

    /// The title to show: the header's, else the file name without extension.
    fn display_title(&self, path: &Path) -> String {
        let header = clean_title(&self.header_title);
        if !header.is_empty() {
            return header;
        }
        let stem = path
            .file_stem()
            .map(|s| clean_title(&s.to_string_lossy()))
            .unwrap_or_default();
        if stem.is_empty() {
            "Untitled".to_owned()
        } else {
            stem
        }
    }
}

/// `text` trimmed, control characters replaced by `?`, cut to [`MAX_TITLE_CHARS`].
fn clean_title(text: &str) -> String {
    text.trim()
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .take(MAX_TITLE_CHARS)
        .collect()
}

fn has_rom_extension(path: &Path) -> bool {
    path.extension().is_some_and(|e| {
        ROM_EXTENSIONS
            .iter()
            .any(|known| e.eq_ignore_ascii_case(known))
    })
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ---------------------------------------------------------------------------
// The index file
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Record {
    id: String,
    title: String,
    system: String,
    source: String,
    rom: String,
    hash: String,
    added: u64,
    #[serde(default)]
    last_played: Option<u64>,
    /// Bundled games: size and mtime (nanoseconds since the epoch) of the ROM file
    /// when it was last read. Absent in older indexes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rom_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rom_mtime_ns: Option<u64>,
}

#[derive(Serialize)]
struct IndexOut<'a> {
    version: u32,
    games: &'a [Record],
}

#[derive(Deserialize)]
struct IndexIn {
    version: u32,
    /// Kept as raw values so one bad entry does not discard the rest.
    games: Vec<serde_json::Value>,
}

/// The file name of the ROM copy of imported game `id`.
fn rom_file_name(id: &GameId, game: GameSystem) -> String {
    format!("{id}.{}", game.extension())
}

fn thumbnail_path(data: &DataDir, id: &GameId) -> PathBuf {
    data.games().join(format!("{id}.png"))
}

fn existing_thumbnail(data: &DataDir, id: &GameId) -> Option<PathBuf> {
    Some(thumbnail_path(data, id)).filter(|p| p.is_file())
}

impl Record {
    fn from_entry(entry: &Entry) -> Self {
        let rom = match entry.source {
            Source::Imported => rom_file_name(&entry.id, entry.system),
            Source::Bundled => entry.rom_path.to_string_lossy().into_owned(),
        };
        Self {
            id: entry.id.to_string(),
            title: entry.title.clone(),
            system: entry.system.extension().to_owned(),
            source: entry.source.name().to_owned(),
            rom,
            hash: format!("{:016x}", entry.id.hash()),
            added: entry.added,
            last_played: entry.last_played,
            rom_size: entry.stamp.map(|s| s.size),
            rom_mtime_ns: entry.stamp.map(|s| s.mtime_ns),
        }
    }

    /// Check a record read from the file and make the entry, or say what is wrong.
    fn into_entry(self, data: &DataDir) -> Result<Entry, String> {
        let id = GameId::parse(&self.id).ok_or_else(|| format!("bad id {:?}", self.id))?;
        if self.hash.len() != 16 || format!("{:016x}", id.hash()) != self.hash {
            return Err(format!("{id}: the hash does not match the id"));
        }
        let system = GameSystem::from_extension(&self.system)
            .ok_or_else(|| format!("{id}: unknown system {:?}", self.system))?;
        let source = Source::from_name(&self.source)
            .ok_or_else(|| format!("{id}: unknown source {:?}", self.source))?;
        let rom_path = match source {
            Source::Imported => {
                if self.rom != rom_file_name(&id, system) {
                    return Err(format!("{id}: unexpected ROM file {:?}", self.rom));
                }
                data.games().join(&self.rom)
            }
            Source::Bundled => {
                let path = PathBuf::from(&self.rom);
                if self.rom.chars().count() > MAX_PATH_CHARS || !path.is_absolute() {
                    return Err(format!("{id}: bad bundled ROM path"));
                }
                path
            }
        };
        let title = clean_title(&self.title);
        let stamp = match (source, self.rom_size, self.rom_mtime_ns) {
            (Source::Bundled, Some(size), Some(mtime_ns)) => Some(RomStamp { size, mtime_ns }),
            _ => None,
        };
        Ok(Entry {
            stamp,
            available: rom_path.is_file(),
            thumbnail: existing_thumbnail(data, &id),
            id,
            title: if title.is_empty() {
                "Untitled".to_owned()
            } else {
                title
            },
            system,
            source,
            rom_path,
            added: self.added,
            last_played: self.last_played,
        })
    }
}

/// Keep the index file under a name nothing else has (`library.json.bak`,
/// `.bak1`, ...) before it is replaced. A corrupt index is moved (`keep_original`
/// false: rename, so a huge or unreadable file is never read into memory); a
/// good one that is about to lose entries is copied, streamed by the OS, and stays
/// until the new one replaces it.
fn backup_index(data: &DataDir, keep_original: bool) -> io::Result<PathBuf> {
    let src = data.index_file();
    for n in 0..=1000u32 {
        let candidate = if n == 0 {
            append_ext(&src, "bak")
        } else {
            append_ext(&src, &format!("bak{n}"))
        };
        if fs::symlink_metadata(&candidate).is_ok() {
            continue;
        }
        if keep_original {
            fs::copy(&src, &candidate)?;
        } else {
            fs::rename(&src, &candidate)?;
        }
        return Ok(candidate);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "1000 backups of the library index already exist",
    ))
}

/// [`backup_index`], with the outcome in the log.
fn backup_index_logged(data: &DataDir, keep_original: bool, why: &str) {
    match backup_index(data, keep_original) {
        Ok(copy) => log::warn!(
            "library: {why}; the old index was kept as {}",
            copy.display()
        ),
        Err(e) => log::warn!("library: {why}; cannot keep a copy of the old index: {e}"),
    }
}

/// What reading the index gave.
struct Loaded {
    entries: Vec<Entry>,
    /// Entries that were present but unusable and were left out.
    dropped: usize,
}

/// Read the index: `Ok(None)` if there is none, `Err` if the file cannot be used
/// as a whole (unreadable, too big, not JSON, wrong shape, another version).
fn load_index(data: &DataDir) -> Result<Option<Loaded>, String> {
    let path = data.index_file();
    let bytes = match read_capped(&path, MAX_INDEX_BYTES, "library index") {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let index: IndexIn = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not a library index: {e}", path.display()))?;
    if index.version != INDEX_VERSION {
        return Err(format!(
            "{} is version {}, this program reads version {INDEX_VERSION}",
            path.display(),
            index.version
        ));
    }
    let mut entries: Vec<Entry> = Vec::new();
    let mut dropped = index.games.len().saturating_sub(MAX_ENTRIES);
    for value in index.games.into_iter().take(MAX_ENTRIES) {
        let entry = serde_json::from_value::<Record>(value)
            .map_err(|e| e.to_string())
            .and_then(|record| record.into_entry(data));
        match entry {
            Ok(entry) if entries.iter().any(|e| e.id == entry.id) => {
                log::warn!("library: ignoring a repeated entry for {}", entry.id);
                dropped += 1;
            }
            Ok(entry) => entries.push(entry),
            Err(why) => {
                log::warn!("library: ignoring an unusable entry: {why}");
                dropped += 1;
            }
        }
    }
    Ok(Some(Loaded { entries, dropped }))
}

/// Add the ROM files in `games/` that no entry covers, such as after the index
/// was lost. A file already named `<id>.<ext>` for a known id is not read. Any
/// other ROM file is checked and renamed to its canonical name (unless that name
/// is taken). Returns how many were added.
fn adopt_orphans(data: &DataDir, entries: &mut Vec<Entry>) -> usize {
    let Ok(listing) = fs::read_dir(data.games()) else {
        return 0;
    };
    let mut files: Vec<PathBuf> = listing
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| has_rom_extension(p))
        .collect();
    files.sort();
    let mut added = 0;
    for path in files {
        let known = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(GameId::parse)
            .is_some_and(|id| entries.iter().any(|e| e.id == id));
        if known || !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let probe = match Probe::read(&path) {
            Ok(p) => p,
            Err(why) => {
                log::warn!("library: not adding {}: {why}", path.display());
                continue;
            }
        };
        let id = probe.id();
        let canonical = data.games().join(rom_file_name(&id, probe.game));
        if entries.iter().any(|e| e.id == id) {
            log::warn!("library: {} repeats {id}; leaving it alone", path.display());
            continue;
        }
        if canonical != path {
            if canonical.exists() {
                log::warn!("library: cannot rename {} to {id}: taken", path.display());
                continue;
            }
            if let Err(e) = fs::rename(&path, &canonical) {
                log::warn!("library: cannot rename {}: {e}", path.display());
                continue;
            }
        }
        let added_at = fs::metadata(&canonical)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or_else(now_secs, |d| d.as_secs());
        entries.push(Entry {
            available: true,
            thumbnail: existing_thumbnail(data, &id),
            title: probe.display_title(&path),
            system: probe.game,
            source: Source::Imported,
            rom_path: canonical,
            added: added_at,
            last_played: None,
            stamp: None,
            id,
        });
        added += 1;
    }
    added
}

// ---------------------------------------------------------------------------
// The library
// ---------------------------------------------------------------------------

/// What [`Library::add_rom`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Added {
    /// The game now in the library (the existing one for a duplicate).
    pub entry: Entry,
    /// The same ROM content was already in the library: nothing was copied.
    pub duplicate: bool,
}

/// What [`Library::register_bundled`] changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BundledReport {
    /// Games newly listed.
    pub added: usize,
    /// Games already listed whose ROM moved to a new path.
    pub updated: usize,
    /// Bundled games no longer in the directory, unlisted. Their saves stay.
    pub removed: usize,
    /// ROM files that could not be used (reported in the log).
    pub skipped: usize,
}

/// Whether [`Library::remove`] also deletes the game's saves, resume point, snapshots and
/// screenshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveSaves {
    Keep,
    Delete,
}

/// The game library of one app folder. Changes are written to the index at once.
/// Each change first re-reads the index, so a second `gb` process adding a game
/// is not undone; two changes at the same instant can still lose one.
pub struct Library {
    data: DataDir,
    entries: Vec<Entry>,
}

impl Library {
    /// Open the library in `data`. Never fails: a missing index is an empty
    /// library (plus any ROMs found in `games/`), and a corrupt one is copied
    /// aside (`library.json.bak`) and rebuilt from the ROMs in `games/`, with the
    /// problem in the log. Does not create the folders ([`DataDir::create`] does).
    pub fn open(data: &DataDir) -> Self {
        let (entries, dropped, damaged) = match load_index(data) {
            Ok(Some(loaded)) => (loaded.entries, loaded.dropped, false),
            Ok(None) => (Vec::new(), 0, false),
            Err(why) => {
                log::warn!(
                    "library: {why}; rebuilding it from {}",
                    data.games().display()
                );
                backup_index_logged(data, false, "the index is unusable");
                (Vec::new(), 0, true)
            }
        };
        let mut library = Self {
            data: data.clone(),
            entries,
        };
        let adopted = adopt_orphans(data, &mut library.entries);
        if adopted > 0 {
            log::info!(
                "library: found {adopted} game(s) in {}",
                data.games().display()
            );
        }
        if dropped > 0 {
            // The rewrite below leaves these entries out; keep what they said.
            backup_index_logged(
                data,
                true,
                &format!("{dropped} unusable entries are left out"),
            );
        }
        if (adopted > 0 || dropped > 0 || damaged)
            && let Err(e) = library.save_index()
        {
            log::warn!("{e}");
        }
        library
    }

    /// The games, in the order they were added.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn get(&self, id: &GameId) -> Option<&Entry> {
        self.entries.iter().find(|e| &e.id == id)
    }

    /// The game whose ROM hashes to `hash`, if listed.
    pub fn find_by_hash(&self, hash: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id.hash() == hash)
    }

    pub fn data_dir(&self) -> &DataDir {
        &self.data
    }

    /// Where the save, resume point, snapshots and screenshots of `id` go. Defined for any id,
    /// listed or not.
    pub fn paths_for(&self, id: &GameId) -> GamePaths {
        GamePaths::new(&self.data, id)
    }

    /// Add the ROM at `path`: check it like `gb` does before running it (size caps,
    /// system detection, the core accepting it), and copy it to
    /// `games/<id>.<gb|gbc|gba>`. If a game with the same content is already listed
    /// nothing is copied and [`Added::duplicate`] is set. A `.sav` and `.ss1`..`.ss9`
    /// beside `path` are imported either way ([`import_beside_rom`]).
    pub fn add_rom(&mut self, path: &Path) -> Result<Added, String> {
        let probe = Probe::read(path)?;
        let id = probe.id();
        self.refresh();
        import_beside_rom(path, &self.paths_for(&id));
        if let Some(index) = self.entries.iter().position(|e| e.id == id) {
            let entry = &mut self.entries[index];
            // Adding the ROM again is how a missing copy comes back.
            if entry.source == Source::Imported && !entry.available {
                write_atomic(&entry.rom_path, &probe.rom)
                    .map_err(|e| format!("cannot copy {} into the library: {e}", path.display()))?;
                entry.available = true;
                log::info!("restored the missing ROM of {id} from {}", path.display());
            } else {
                log::info!("{} is already in the library as {id}", path.display());
            }
            return Ok(Added {
                entry: entry.clone(),
                duplicate: true,
            });
        }
        let dest = self.data.games().join(rom_file_name(&id, probe.game));
        fs::create_dir_all(self.data.games())
            .and_then(|()| write_atomic(&dest, &probe.rom))
            .map_err(|e| format!("cannot copy {} into the library: {e}", path.display()))?;
        let entry = Entry {
            available: true,
            thumbnail: existing_thumbnail(&self.data, &id),
            title: probe.display_title(path),
            system: probe.game,
            source: Source::Imported,
            rom_path: dest.clone(),
            added: now_secs(),
            last_played: None,
            stamp: None,
            id,
        };
        self.entries.push(entry.clone());
        if let Err(e) = self.save_index() {
            self.entries.pop();
            let _ = fs::remove_file(&dest);
            return Err(e);
        }
        log::info!(
            "added {} to the library as {} ({}, {})",
            path.display(),
            entry.id,
            entry.title,
            entry.system.label()
        );
        Ok(Added {
            entry,
            duplicate: false,
        })
    }

    /// Remove game `id` from the library: its ROM copy and thumbnail are deleted
    /// and the entry unlisted. Saves, the resume point, snapshots and screenshots stay unless
    /// `saves` is [`RemoveSaves::Delete`]. A bundled game cannot be removed (its
    /// ROM is not the library's to delete).
    pub fn remove(&mut self, id: &GameId, saves: RemoveSaves) -> Result<Entry, String> {
        self.refresh();
        let index = self
            .entries
            .iter()
            .position(|e| &e.id == id)
            .ok_or_else(|| format!("{id} is not in the library"))?;
        if self.entries[index].source == Source::Bundled {
            return Err(format!(
                "{id} is bundled with the app and cannot be removed"
            ));
        }
        let entry = self.entries.remove(index);
        if let Err(e) = self.save_index() {
            self.entries.insert(index, entry);
            return Err(e);
        }
        // Derived from the id, not taken from the index.
        let files = [
            self.data.games().join(rom_file_name(id, entry.system)),
            thumbnail_path(&self.data, id),
        ];
        for file in files {
            remove_file_logged(&file);
        }
        if saves == RemoveSaves::Delete {
            remove_with_prefix(&self.data.saves(), &format!("{id}."));
            remove_with_prefix(&self.data.states(), &format!("{id}."));
            remove_states_dir(&GamePaths::new(&self.data, id));
            remove_screenshots(&self.data.screenshots(), id);
        }
        log::info!(
            "removed {id} from the library ({})",
            match saves {
                RemoveSaves::Keep => "saves kept",
                RemoveSaves::Delete => "saves deleted",
            }
        );
        Ok(entry)
    }

    /// List the ROMs in `dir` (the app's read-only resources) as [`Source::Bundled`]
    /// games, without copying them. Safe to call at every start: a game already
    /// listed is left as it is (its path is updated if the app moved), one the
    /// player imported themselves stays imported, and a bundled game whose ROM is
    /// not in `dir` is unlisted (its saves stay), so an entry from another folder,
    /// a development build's say, does not linger. A ROM whose path, size and
    /// modification time match its entry is not read again. Unusable files are
    /// skipped and logged.
    pub fn register_bundled(&mut self, dir: &Path) -> Result<BundledReport, String> {
        let dir = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
        let dir = dir.as_path();
        let listing = fs::read_dir(dir)
            .map_err(|e| format!("cannot read the bundled games in {}: {e}", dir.display()))?;
        let mut files: Vec<PathBuf> = listing
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| has_rom_extension(p) && p.is_file())
            .collect();
        files.sort();
        self.refresh();
        let mut report = BundledReport::default();
        // Entries whose stamp changed without the report showing it.
        let mut restamped = false;
        let mut seen: Vec<GameId> = Vec::new();
        // Files that are there but could not be read this time.
        let mut unreadable: Vec<PathBuf> = Vec::new();
        for file in files {
            let path = std::path::absolute(&file).unwrap_or_else(|_| file.clone());
            // Taken before the read: a file that changes meanwhile is read again next time.
            let stamp = RomStamp::of(&file);
            if stamp.is_some()
                && let Some(entry) = self
                    .entries
                    .iter_mut()
                    .find(|e| e.source == Source::Bundled && e.rom_path == path && e.stamp == stamp)
            {
                entry.available = true;
                seen.push(entry.id.clone());
                continue;
            }
            let probe = match Probe::read(&file) {
                Ok(p) => p,
                Err(why) => {
                    log::warn!("bundled game not used: {why}");
                    report.skipped += 1;
                    unreadable.push(path);
                    continue;
                }
            };
            let id = probe.id();
            seen.push(id.clone());
            match self.entries.iter_mut().find(|e| e.id == id) {
                Some(entry) if entry.source == Source::Bundled => {
                    entry.available = true;
                    if entry.rom_path != path {
                        entry.rom_path = path;
                        report.updated += 1;
                    }
                    if entry.stamp != stamp {
                        entry.stamp = stamp;
                        restamped = true;
                    }
                }
                Some(_) => {} // the player already has this game; their copy stays
                None => {
                    self.entries.push(Entry {
                        available: true,
                        thumbnail: existing_thumbnail(&self.data, &id),
                        title: probe.display_title(&file),
                        system: probe.game,
                        source: Source::Bundled,
                        rom_path: path,
                        added: now_secs(),
                        last_played: None,
                        stamp,
                        id,
                    });
                    report.added += 1;
                }
            }
        }
        // A bundled game goes when its ROM is not in `dir` (or `dir` now holds another
        // game under that name). One whose file is there but failed to read this once stays.
        let before = self.entries.len();
        self.entries.retain(|e| {
            e.source != Source::Bundled || seen.contains(&e.id) || unreadable.contains(&e.rom_path)
        });
        report.removed = before - self.entries.len();
        if report != BundledReport::default() || restamped {
            self.save_index()?;
        }
        if report != BundledReport::default() {
            log::info!(
                "bundled games: {} added, {} moved, {} removed, {} skipped",
                report.added,
                report.updated,
                report.removed,
                report.skipped
            );
        }
        Ok(report)
    }

    /// Record that `id` was played just now.
    pub fn touch_played(&mut self, id: &GameId) -> Result<(), String> {
        self.refresh();
        let entry = self
            .entries
            .iter_mut()
            .find(|e| &e.id == id)
            .ok_or_else(|| format!("{id} is not in the library"))?;
        entry.last_played = Some(now_secs());
        self.save_index()
    }

    /// Set the thumbnail of `id` to the PNG in `png` (at most 1 MiB and 1024 px a
    /// side; it is decoded to check it). Returns the file it was written to.
    pub fn set_thumbnail(&mut self, id: &GameId, png: &[u8]) -> Result<PathBuf, String> {
        check_png(png)?;
        self.refresh();
        let index = self
            .entries
            .iter()
            .position(|e| &e.id == id)
            .ok_or_else(|| format!("{id} is not in the library"))?;
        let path = thumbnail_path(&self.data, id);
        write_atomic(&path, png)
            .map_err(|e| format!("cannot write thumbnail {}: {e}", path.display()))?;
        self.entries[index].thumbnail = Some(path.clone());
        Ok(path)
    }

    /// Whether opening `id` would continue from a resume point.
    pub fn has_resume(&self, id: &GameId) -> bool {
        self.paths_for(id).resume_state().is_file()
    }

    /// The thumbnail PNG of `id`, if it has one that can be read.
    pub fn read_thumbnail(&self, id: &GameId) -> Option<Vec<u8>> {
        read_capped(
            &thumbnail_path(&self.data, id),
            MAX_THUMBNAIL_BYTES,
            "thumbnail",
        )
        .ok()
        .flatten()
    }

    /// Replace the entries with what the index holds now. The next change rewrites
    /// the file, so an index that cannot be used (corrupt, or written by a newer
    /// version) or that has entries this version drops is backed up first; the
    /// entries in memory stay in the first case.
    fn refresh(&mut self) {
        match load_index(&self.data) {
            Ok(Some(loaded)) => {
                if loaded.dropped > 0 {
                    backup_index_logged(
                        &self.data,
                        true,
                        &format!("{} unusable entries are left out", loaded.dropped),
                    );
                }
                self.entries = loaded.entries;
            }
            Ok(None) => {}
            Err(why) => backup_index_logged(&self.data, false, &why),
        }
    }

    fn save_index(&self) -> Result<(), String> {
        let records: Vec<Record> = self.entries.iter().map(Record::from_entry).collect();
        let json = serde_json::to_vec_pretty(&IndexOut {
            version: INDEX_VERSION,
            games: &records,
        })
        .map_err(|e| format!("cannot encode the library index: {e}"))?;
        let path = self.data.index_file();
        fs::create_dir_all(self.data.games())
            .and_then(|()| write_atomic(&path, &json))
            .map_err(|e| format!("cannot write the library index {}: {e}", path.display()))
    }
}

/// Check `png` is a PNG within the thumbnail limits by decoding it.
fn check_png(png: &[u8]) -> Result<(), String> {
    if png.len() as u64 > MAX_THUMBNAIL_BYTES {
        return Err(format!(
            "thumbnail is larger than {MAX_THUMBNAIL_BYTES} bytes"
        ));
    }
    let bad = |e: &dyn fmt::Display| format!("thumbnail is not a usable PNG: {e}");
    let mut reader = png::Decoder::new(Cursor::new(png))
        .read_info()
        .map_err(|e| bad(&e))?;
    let (width, height) = (reader.info().width, reader.info().height);
    if width == 0 || height == 0 || width > MAX_THUMBNAIL_SIDE || height > MAX_THUMBNAIL_SIDE {
        return Err(format!(
            "thumbnail is {width}x{height}, at most {MAX_THUMBNAIL_SIDE}x{MAX_THUMBNAIL_SIDE} is allowed"
        ));
    }
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| bad(&"image too large"))?;
    let mut pixels = vec![0u8; size];
    reader.next_frame(&mut pixels).map_err(|e| bad(&e))?;
    Ok(())
}

fn remove_file_logged(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("cannot delete {}: {e}", path.display()),
    }
}

/// Whether `rest` is what follows `<id>-` in a screenshot name:
/// `YYYYMMDD-HHMMSS-fNNNNNNN.png`.
fn is_screenshot_suffix(rest: &str) -> bool {
    let b = rest.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    b.len() == 28
        && digits(0..8)
        && b[8] == b'-'
        && digits(9..15)
        && b[15] == b'-'
        && b[16] == b'f'
        && digits(17..24)
        && &b[24..] == b".png"
}

/// Delete the screenshots of `id` in `dir`. The whole name is checked, so another
/// game whose id merely starts with this one's is not touched.
fn remove_screenshots(dir: &Path, id: &GameId) {
    let Ok(listing) = fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{id}-");
    for entry in listing.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ours = name.strip_prefix(&prefix).is_some_and(is_screenshot_suffix);
        if ours && entry.file_type().is_ok_and(|t| t.is_file()) {
            remove_file_logged(&entry.path());
        }
    }
}

/// Delete the resume point and snapshots of a game: the recognised files in its states
/// folder, then the folder if nothing else is left in it. A folder that is a symbolic
/// link is never followed.
fn remove_states_dir(paths: &GamePaths) {
    let dir = paths.states_dir();
    if !fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return;
    }
    let Ok(listing) = fs::read_dir(dir) else {
        return;
    };
    for entry in listing.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if state::is_state_file_name(&name) && entry.file_type().is_ok_and(|t| t.is_file()) {
            remove_file_logged(&entry.path());
        }
    }
    // Fails, harmlessly, if the player put something else in there.
    let _ = fs::remove_dir(dir);
}

/// Delete the regular files in `dir` whose name starts with `prefix`.
fn remove_with_prefix(dir: &Path, prefix: &str) {
    let Ok(listing) = fs::read_dir(dir) else {
        return;
    };
    for entry in listing.filter_map(Result::ok) {
        let matches = entry.file_name().to_string_lossy().starts_with(prefix);
        if matches && entry.file_type().is_ok_and(|t| t.is_file()) {
            remove_file_logged(&entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn mtime_of(path: &Path) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    /// A Game Boy ROM: ROM-only cartridge with `title` in the header.
    fn gb_rom(title: &str, fill: u8) -> Vec<u8> {
        let mut rom = vec![fill; 0x8000];
        rom[0x100..0x150].fill(0);
        rom[0x134..0x134 + title.len()].copy_from_slice(title.as_bytes());
        rom[0x143] = 0; // not CGB
        rom[0x147] = 0x00;
        rom
    }

    fn gbc_rom(title: &str) -> Vec<u8> {
        let mut rom = gb_rom(title, 0);
        rom[0x143] = 0x80;
        rom
    }

    fn gba_rom(title: &str) -> Vec<u8> {
        let mut rom = vec![0u8; 0x200];
        rom[..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
        rom[0xA0..0xA0 + title.len()].copy_from_slice(title.as_bytes());
        rom
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        data: DataDir,
        /// A folder with the player's own ROM files.
        roms: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDir::at(tmp.path().join("app"));
        data.create().unwrap();
        let roms = tmp.path().join("roms");
        fs::create_dir_all(&roms).unwrap();
        Fixture {
            _tmp: tmp,
            data,
            roms,
        }
    }

    fn write_rom(dir: &Path, name: &str, rom: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, rom).unwrap();
        path
    }

    #[test]
    fn ids_are_a_readable_slug_and_the_full_hash() {
        let id = GameId::new("Moth Quest!", 0x1a2b3c4d);
        assert_eq!(id.as_str(), "moth-quest-000000001a2b3c4d");
        assert_eq!(id.hash(), 0x1a2b3c4d);
        assert_eq!(GameId::new("", 1).as_str(), "game-0000000000000001");
        assert_eq!(GameId::new("  --  ", 1).as_str(), "game-0000000000000001");
        assert_eq!(GameId::new("é", 1).as_str(), "game-0000000000000001");
        let long = GameId::new(&"A".repeat(100), u64::MAX);
        assert_eq!(long.as_str().len(), MAX_SLUG_CHARS + 1 + 16);
        assert_eq!(GameId::parse(id.as_str()), Some(id));
    }

    #[test]
    fn ids_that_could_escape_a_folder_do_not_parse() {
        for bad in [
            "",
            "-0000000000000001",
            "a-1",
            "../x-0000000000000001",
            "a/b-0000000000000001",
            "a\\b-0000000000000001",
            "A-0000000000000001",
            "a--b-0000000000000001",
            "a--0000000000000001",
            "a-000000000000000G",
            "a-0000000000000001.gb",
        ] {
            assert_eq!(GameId::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn save_paths_are_keyed_by_the_hash_not_the_rom_name() {
        let data = DataDir::at("/app");
        let rom = gb_rom("MOTH", 0);
        let id = GameId::new("MOTH", rom_hash(&rom));
        let p = GamePaths::new(&data, &id);
        let name = id.as_str();
        assert_eq!(p.sav, PathBuf::from(format!("/app/saves/{name}.sav")));
        assert_eq!(
            p.resume_state(),
            PathBuf::from(format!("/app/states/{name}/resume.state"))
        );
        assert_eq!(
            p.snapshot_state(1_700_000_000),
            PathBuf::from(format!("/app/states/{name}/snap-1700000000.state"))
        );
        assert_eq!(
            p.snapshot_image(1_700_000_000),
            PathBuf::from(format!("/app/states/{name}/snap-1700000000.png"))
        );
        assert_eq!(
            p.legacy_state(3),
            PathBuf::from(format!("/app/states/{name}.ss3"))
        );
        assert_eq!(
            p.screenshot(1_700_000_000, 1234),
            PathBuf::from(format!(
                "/app/screenshots/{name}-20231114-221320-f0001234.png"
            ))
        );
        // Other content gives other files; the same content the same ones.
        let other = GameId::new("MOTH", rom_hash(&gb_rom("MOTH", 1)));
        assert_ne!(GamePaths::new(&data, &other).sav, p.sav);
        assert_eq!(GamePaths::new(&data, &id), p);
    }

    #[test]
    fn the_hash_is_the_one_the_cores_compute() {
        let gb = gb_rom("MOTH", 7);
        assert_eq!(rom_hash(&gb), GameBoy::new(gb.clone()).unwrap().rom_hash());
        let gba = gba_rom("MOTHGBA");
        assert_eq!(
            rom_hash(&gba),
            Gba::with_options(gba.clone(), None, CartOptions::default())
                .unwrap()
                .rom_hash()
        );
    }

    #[test]
    fn add_copies_the_rom_in_and_lists_it() {
        let f = fixture();
        let src = write_rom(&f.roms, "whatever name.gb", &gb_rom("MOTH", 0));
        let mut lib = Library::open(&f.data);
        assert!(lib.entries().is_empty());
        let added = lib.add_rom(&src).unwrap();
        assert!(!added.duplicate);
        let e = added.entry;
        assert_eq!(e.title, "MOTH");
        assert_eq!(e.system, GameSystem::Gb);
        assert_eq!(e.source, Source::Imported);
        assert_eq!(e.last_played, None);
        assert_eq!(e.thumbnail, None);
        assert_eq!(e.rom_path, f.data.games().join(format!("{}.gb", e.id)));
        assert_eq!(fs::read(&e.rom_path).unwrap(), fs::read(&src).unwrap());
        // It survives a restart.
        let again = Library::open(&f.data);
        assert_eq!(again.entries(), &[e]);
    }

    #[test]
    fn systems_come_from_the_existing_detection() {
        let f = fixture();
        let mut lib = Library::open(&f.data);
        let gb = lib
            .add_rom(&write_rom(&f.roms, "a.gb", &gb_rom("A", 0)))
            .unwrap();
        let gbc = lib
            .add_rom(&write_rom(&f.roms, "b.gbc", &gbc_rom("B")))
            .unwrap();
        let gba = lib
            .add_rom(&write_rom(&f.roms, "c.gba", &gba_rom("C")))
            .unwrap();
        // A GBA header is found without the extension too.
        let mut headered = gba_rom("D");
        headered[0xB2] = 0x96;
        let sum = headered[0xA0..=0xBC]
            .iter()
            .fold(0u8, |a, &b| a.wrapping_add(b));
        headered[0xBD] = 0u8.wrapping_sub(sum).wrapping_sub(0x19);
        let sniffed = lib
            .add_rom(&write_rom(&f.roms, "d.bin", &headered))
            .unwrap();
        assert_eq!(gb.entry.system, GameSystem::Gb);
        assert_eq!(gbc.entry.system, GameSystem::Gbc);
        assert_eq!(gba.entry.system, GameSystem::Gba);
        assert_eq!(sniffed.entry.system, GameSystem::Gba);
        assert!(gbc.entry.rom_path.extension().unwrap() == "gbc");
        assert!(sniffed.entry.rom_path.extension().unwrap() == "gba");
        assert_eq!(GameSystem::Gbc.system(), System::Gb);
        assert_eq!(GameSystem::Gbc.label(), "GBC");
    }

    #[test]
    fn a_title_less_rom_is_named_after_its_file() {
        let f = fixture();
        let mut lib = Library::open(&f.data);
        let e = lib
            .add_rom(&write_rom(&f.roms, "Some Game.gb", &gb_rom("", 0)))
            .unwrap()
            .entry;
        assert_eq!(e.title, "Some Game");
        assert!(e.id.as_str().starts_with("game-"));
    }

    #[test]
    fn adding_the_same_content_again_copies_nothing() {
        let f = fixture();
        let a = write_rom(&f.roms, "a.gb", &gb_rom("MOTH", 0));
        let b = write_rom(&f.roms, "renamed copy.gb", &gb_rom("MOTH", 0));
        let mut lib = Library::open(&f.data);
        let first = lib.add_rom(&a).unwrap();
        let second = lib.add_rom(&b).unwrap();
        assert!(second.duplicate);
        assert_eq!(second.entry, first.entry);
        assert_eq!(lib.entries().len(), 1);
        let files = fs::read_dir(f.data.games()).unwrap().count();
        assert_eq!(files, 2, "one ROM and the index");
    }

    #[test]
    fn bad_roms_are_refused_with_the_frontends_messages() {
        let f = fixture();
        let mut lib = Library::open(&f.data);
        let empty = write_rom(&f.roms, "empty.gb", b"");
        assert!(
            lib.add_rom(&empty)
                .unwrap_err()
                .contains("the file is empty")
        );
        let zip = write_rom(
            &f.roms,
            "z.gb",
            b"PK\x03\x04 and so on, padded out to look big enough",
        );
        assert!(lib.add_rom(&zip).unwrap_err().contains("zip archive"));
        let missing = f.roms.join("nope.gb");
        assert!(
            lib.add_rom(&missing)
                .unwrap_err()
                .contains("file not found")
        );
        let dir_err = lib.add_rom(&f.roms).unwrap_err();
        assert!(dir_err.contains("directory"), "{dir_err}");
        let mut huge = gb_rom("BIG", 0);
        huge.resize(8 * 1024 * 1024 + 4096, 0);
        let huge = write_rom(&f.roms, "huge.gb", &huge);
        assert!(lib.add_rom(&huge).unwrap_err().contains("8 MiB"));
        assert!(lib.entries().is_empty());
        assert!(!f.data.index_file().exists());
    }

    #[test]
    fn remove_deletes_the_rom_and_keeps_the_saves_by_default() {
        let f = fixture();
        let mut lib = Library::open(&f.data);
        let e = lib
            .add_rom(&write_rom(&f.roms, "a.gb", &gb_rom("MOTH", 0)))
            .unwrap()
            .entry;
        let paths = lib.paths_for(&e.id);
        fs::write(&paths.sav, b"save").unwrap();
        fs::create_dir_all(paths.states_dir()).unwrap();
        fs::write(paths.resume_state(), b"state").unwrap();
        fs::write(paths.screenshot(0, 5), b"png").unwrap();
        lib.set_thumbnail(&e.id, &tiny_png()).unwrap();

        let removed = lib.remove(&e.id, RemoveSaves::Keep).unwrap();
        assert_eq!(removed.id, e.id);
        assert!(lib.entries().is_empty());
        assert!(!e.rom_path.exists());
        assert!(!thumbnail_path(&f.data, &e.id).exists());
        assert!(
            paths.sav.exists() && paths.resume_state().exists() && paths.screenshot(0, 5).exists()
        );
        assert!(Library::open(&f.data).entries().is_empty());
        assert!(lib.remove(&e.id, RemoveSaves::Keep).is_err());
    }

    #[test]
    fn remove_can_delete_this_games_files_and_only_those() {
        let f = fixture();
        let mut lib = Library::open(&f.data);
        let e = lib
            .add_rom(&write_rom(&f.roms, "a.gb", &gb_rom("MOTH", 0)))
            .unwrap()
            .entry;
        let other = lib
            .add_rom(&write_rom(&f.roms, "b.gb", &gb_rom("MOTH", 1)))
            .unwrap()
            .entry;
        for id in [&e.id, &other.id] {
            let p = lib.paths_for(id);
            fs::write(&p.sav, b"s").unwrap();
            fs::write(append_ext(&p.sav, "bak"), b"s").unwrap();
            fs::create_dir_all(p.states_dir()).unwrap();
            fs::write(p.resume_state(), b"s").unwrap();
            fs::write(p.resume_bad(), b"s").unwrap();
            fs::write(p.snapshot_state(5), b"s").unwrap();
            fs::write(p.snapshot_image(5), b"s").unwrap();
            fs::write(p.legacy_state(9), b"s").unwrap();
            fs::write(p.screenshot(0, 1), b"s").unwrap();
        }
        lib.remove(&e.id, RemoveSaves::Delete).unwrap();
        let gone = lib.paths_for(&e.id);
        for path in [
            gone.sav.clone(),
            append_ext(&gone.sav, "bak"),
            gone.resume_state(),
            gone.resume_bad(),
            gone.snapshot_state(5),
            gone.snapshot_image(5),
            gone.states_dir().to_owned(),
            gone.legacy_state(9),
            gone.screenshot(0, 1),
        ] {
            assert!(!path.exists(), "{}", path.display());
        }
        let kept = lib.paths_for(&other.id);
        assert!(
            kept.sav.exists() && kept.legacy_state(9).exists() && kept.screenshot(0, 1).exists()
        );
        assert!(kept.resume_state().exists() && kept.snapshot_state(5).exists());
        assert!(kept.snapshot_image(5).exists());
        assert!(lib.get(&other.id).is_some());
    }

    #[test]
    fn a_hostile_index_cannot_make_remove_delete_other_files() {
        let f = fixture();
        let victim = f.roms.join("precious.txt");
        fs::write(&victim, b"keep me").unwrap();
        let mut lib = Library::open(&f.data);
        let e = lib
            .add_rom(&write_rom(&f.roms, "a.gb", &gb_rom("MOTH", 0)))
            .unwrap()
            .entry;
        // Point the entry's ROM at a file elsewhere.
        let index = fs::read_to_string(f.data.index_file()).unwrap();
        let hostile = index.replace(
            &format!("{}.gb\"", e.id),
            &format!("{}\"", victim.display()),
        );
        fs::write(f.data.index_file(), hostile).unwrap();
        let mut lib = Library::open(&f.data);
        assert!(
            lib.get(&e.id).is_some(),
            "the ROM copy is found again by scanning"
        );
        lib.remove(&e.id, RemoveSaves::Delete).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"keep me");
    }

    #[test]
    fn a_corrupt_index_is_kept_aside_and_rebuilt_from_the_roms() {
        let f = fixture();
        let mut lib = Library::open(&f.data);
        let moth = lib
            .add_rom(&write_rom(&f.roms, "a.gb", &gb_rom("MOTH", 0)))
            .unwrap()
            .entry;
        let gba = lib
            .add_rom(&write_rom(&f.roms, "b.gba", &gba_rom("GBAGAME")))
            .unwrap()
            .entry;
        for garbage in [
            &b"{ not json"[..],
            b"",
            b"[]",
            b"{\"version\":99,\"games\":[]}",
            b"{\"version\":1}",
            &[0xFF, 0xFE, 0x00, 0x80],
        ] {
            fs::write(f.data.index_file(), garbage).unwrap();
            let rebuilt = Library::open(&f.data);
            assert_eq!(rebuilt.entries().len(), 2);
            assert!(rebuilt.get(&moth.id).is_some() && rebuilt.get(&gba.id).is_some());
            // The rebuilt index is valid again, and the bad one was kept.
            assert!(load_index(&f.data).unwrap().is_some());
        }
        let backups = fs::read_dir(f.data.games())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("library.json.bak")
            })
            .count();
        assert!(backups >= 1);
    }

    #[test]
    fn a_missing_index_is_rebuilt_and_loose_roms_are_adopted_under_their_canonical_name() {
        let f = fixture();
        let rom = gb_rom("LOOSE", 3);
        write_rom(&f.data.games(), "dropped in by hand.gb", &rom);
        write_rom(&f.data.games(), "notes.txt", b"not a rom");
        write_rom(&f.data.games(), "broken.gb", b"junk");
        let lib = Library::open(&f.data);
        assert_eq!(lib.entries().len(), 1);
        let e = &lib.entries()[0];
        assert_eq!(e.title, "LOOSE");
        assert_eq!(
            e.rom_path.file_name().unwrap().to_string_lossy(),
            format!("{}.gb", e.id)
        );
        assert_eq!(fs::read(&e.rom_path).unwrap(), rom);
        assert!(f.data.index_file().exists());
        assert!(
            f.data.games().join("broken.gb").exists(),
            "unusable files are left alone"
        );
    }

    #[test]
    fn bad_entries_are_dropped_one_by_one() {
        let f = fixture();
        let mut lib = Library::open(&f.data);
        let good = lib
            .add_rom(&write_rom(&f.roms, "a.gb", &gb_rom("MOTH", 0)))
            .unwrap()
            .entry;
        let good_json = serde_json::to_value(Record::from_entry(&good)).unwrap();
        let mut wrong_hash = good_json.clone();
        wrong_hash["hash"] = "0000000000000000".into();
        let mut path_id = good_json.clone();
        path_id["id"] = "../../etc/passwd".into();
        let mut no_rom = good_json.clone();
        no_rom["id"] = "gone-0000000000000001".into();
        no_rom["hash"] = "0000000000000001".into();
        no_rom["rom"] = "gone-0000000000000001.gb".into();
        let mut relative_bundle = good_json.clone();
        relative_bundle["id"] = "rel-0000000000000002".into();
        relative_bundle["hash"] = "0000000000000002".into();
        relative_bundle["source"] = "bundled".into();
        relative_bundle["rom"] = "relative/path.gb".into();
        let mut huge_added = good_json.clone();
        huge_added["added"] = "not a number".into();
        let doc = serde_json::json!({
            "version": 1,
            "games": [wrong_hash, 5, path_id, no_rom, relative_bundle, huge_added, good_json.clone(), good_json],
        });
        fs::write(f.data.index_file(), doc.to_string()).unwrap();
        let original = fs::read(f.data.index_file()).unwrap();
        let lib = Library::open(&f.data);
        // The good entry and the one whose ROM file is missing stay; the rest go.
        assert_eq!(lib.entries().len(), 2);
        assert!(lib.get(&good.id).unwrap().available);
        let gone = lib
            .entries()
            .iter()
            .find(|e| e.id.as_str() == "gone-0000000000000001")
            .unwrap();
        assert!(!gone.available);
        // The index as it was before the entries were dropped is kept.
        assert_eq!(
            fs::read(f.data.games().join("library.json.bak")).unwrap(),
            original
        );
        assert_eq!(Library::open(&f.data).entries().len(), 2);
    }

    #[test]
    fn a_game_with_a_missing_rom_stays_listed_and_comes_back_when_added_again() {
        let f = fixture();
        let src = write_rom(&f.roms, "a.gb", &gb_rom("MOTH", 0));
        let mut lib = Library::open(&f.data);
        let e = lib.add_rom(&src).unwrap().entry;
        lib.touch_played(&e.id).unwrap();
        fs::remove_file(&e.rom_path).unwrap();

        let lib = Library::open(&f.data);
        let gone = lib.get(&e.id).expect("still listed");
        assert!(!gone.available);
        assert!(gone.last_played.is_some(), "its history is kept");
        assert!(
            !f.data.games().join("library.json.bak").exists(),
            "nothing was dropped, so nothing needs a backup"
        );

        let mut lib = lib;
        let again = lib.add_rom(&src).unwrap();
        assert!(again.duplicate && again.entry.available);
        assert_eq!(fs::read(&e.rom_path).unwrap(), fs::read(&src).unwrap());
        assert!(Library::open(&f.data).get(&e.id).unwrap().available);
    }

    #[test]
    fn changing_a_library_whose_index_went_bad_keeps_the_bad_file_and_the_games() {
        let f = fixture();
        let mut lib = Library::open(&f.data);
        let e = lib
            .add_rom(&write_rom(&f.roms, "a.gb", &gb_rom("MOTH", 0)))
            .unwrap()
            .entry;
        // Another program version wrote something this one cannot read.
        let newer = br#"{"version":2,"games":[{"future":true}]}"#;
        fs::write(f.data.index_file(), newer).unwrap();
        lib.touch_played(&e.id).unwrap();
        assert_eq!(
            fs::read(f.data.games().join("library.json.bak")).unwrap(),
            newer
        );
        let reopened = Library::open(&f.data);
        assert!(reopened.get(&e.id).unwrap().last_played.is_some());
    }

    #[test]
    fn a_change_that_would_drop_entries_backs_up_the_index_first() {
        let f = fixture();
        let mut lib = Library::open(&f.data);
        let e = lib
            .add_rom(&write_rom(&f.roms, "a.gb", &gb_rom("MOTH", 0)))
            .unwrap()
            .entry;
        let mut bad = serde_json::to_value(Record::from_entry(&e)).unwrap();
        bad["id"] = "!!".into();
        let doc = serde_json::json!({
            "version": 1,
            "games": [serde_json::to_value(Record::from_entry(&e)).unwrap(), bad],
        })
        .to_string();
        fs::write(f.data.index_file(), &doc).unwrap();
        lib.touch_played(&e.id).unwrap();
        assert_eq!(
            fs::read_to_string(f.data.games().join("library.json.bak")).unwrap(),
            doc
        );
        assert_eq!(Library::open(&f.data).entries().len(), 1);
    }

    #[test]
    fn titles_from_the_index_are_cleaned() {
        let f = fixture();
        let mut lib = Library::open(&f.data);
        let e = lib
            .add_rom(&write_rom(&f.roms, "a.gb", &gb_rom("MOTH", 0)))
            .unwrap()
            .entry;
        let mut record = serde_json::to_value(Record::from_entry(&e)).unwrap();
        record["title"] = format!("a\nb{}", "x".repeat(500)).into();
        let doc = serde_json::json!({"version": 1, "games": [record]});
        fs::write(f.data.index_file(), doc.to_string()).unwrap();
        let lib = Library::open(&f.data);
        let title = &lib.entries()[0].title;
        assert!(title.starts_with("a?b") && title.chars().count() == MAX_TITLE_CHARS);
    }

    #[test]
    fn an_oversized_index_is_not_read() {
        let f = fixture();
        write_rom(
            &f.data.games(),
            "library.json",
            &vec![b' '; MAX_INDEX_BYTES as usize + 1],
        );
        assert!(load_index(&f.data).is_err());
        assert!(Library::open(&f.data).entries().is_empty());
    }

    fn bundle_dir(f: &Fixture) -> PathBuf {
        let dir = f.roms.parent().unwrap().join("resources/games");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bundled_roms_are_listed_in_place_and_registering_is_idempotent() {
        let f = fixture();
        let dir = bundle_dir(&f);
        write_rom(&dir, "one.gb", &gb_rom("ONE", 0));
        write_rom(&dir, "two.gba", &gba_rom("TWO"));
        write_rom(&dir, "readme.txt", b"hello");
        write_rom(&dir, "bad.gb", b"junk");
        let mut lib = Library::open(&f.data);
        let first = lib.register_bundled(&dir).unwrap();
        assert_eq!(
            first,
            BundledReport {
                added: 2,
                skipped: 1,
                ..Default::default()
            }
        );
        assert_eq!(lib.entries().len(), 2);
        for e in lib.entries() {
            assert_eq!(e.source, Source::Bundled);
            assert!(e.rom_path.starts_with(std::path::absolute(&dir).unwrap()));
        }
        assert!(
            fs::read_dir(f.data.games()).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".gb")),
            "bundled ROMs are not copied"
        );
        let snapshot = lib.entries().to_vec();
        let again = lib.register_bundled(&dir).unwrap();
        assert_eq!(
            again,
            BundledReport {
                skipped: 1,
                ..Default::default()
            }
        );
        assert_eq!(lib.entries(), snapshot);
        // Persisted, and still there after a restart.
        assert_eq!(Library::open(&f.data).entries(), snapshot);
    }

    #[test]
    fn bundled_games_keep_their_history_when_the_app_moves() {
        let f = fixture();
        let old = bundle_dir(&f);
        write_rom(&old, "one.gb", &gb_rom("ONE", 0));
        let mut lib = Library::open(&f.data);
        lib.register_bundled(&old).unwrap();
        let id = lib.entries()[0].id.clone();
        lib.touch_played(&id).unwrap();
        let played = lib.get(&id).unwrap().last_played;
        assert!(played.is_some());

        let new = f.roms.parent().unwrap().join("moved-app/games");
        fs::create_dir_all(&new).unwrap();
        write_rom(&new, "one.gb", &gb_rom("ONE", 0));
        let report = lib.register_bundled(&new).unwrap();
        assert_eq!(report.updated, 1);
        assert_eq!(report.added, 0);
        assert_eq!(lib.entries().len(), 1);
        assert!(
            lib.get(&id)
                .unwrap()
                .rom_path
                .starts_with(std::path::absolute(&new).unwrap())
        );
        assert_eq!(lib.get(&id).unwrap().last_played, played);
    }

    #[test]
    fn a_bundled_game_gone_from_the_app_is_unlisted_but_keeps_its_saves() {
        let f = fixture();
        let dir = bundle_dir(&f);
        let rom = write_rom(&dir, "one.gb", &gb_rom("ONE", 0));
        write_rom(&dir, "two.gb", &gb_rom("TWO", 0));
        let mut lib = Library::open(&f.data);
        lib.register_bundled(&dir).unwrap();
        let one = lib
            .entries()
            .iter()
            .find(|e| e.title == "ONE")
            .unwrap()
            .id
            .clone();
        fs::write(lib.paths_for(&one).sav, b"progress").unwrap();
        fs::remove_file(rom).unwrap();
        let report = lib.register_bundled(&dir).unwrap();
        assert_eq!(report.removed, 1);
        assert!(lib.get(&one).is_none());
        assert_eq!(fs::read(lib.paths_for(&one).sav).unwrap(), b"progress");
        assert!(lib.register_bundled(&f.roms.join("no/such/dir")).is_err());
    }

    #[test]
    fn a_bundled_game_whose_file_fails_to_read_once_stays_listed() {
        let f = fixture();
        let dir = bundle_dir(&f);
        let rom = write_rom(&dir, "one.gb", &gb_rom("ONE", 0));
        write_rom(&dir, "two.gb", &gb_rom("TWO", 0));
        let mut lib = Library::open(&f.data);
        lib.register_bundled(&dir).unwrap();
        let one = lib
            .entries()
            .iter()
            .find(|e| e.title == "ONE")
            .unwrap()
            .clone();

        // The file is there but unreadable as a ROM (a partial update, say).
        fs::write(&rom, b"junk").unwrap();
        let report = lib.register_bundled(&dir).unwrap();
        assert_eq!((report.skipped, report.removed), (1, 0));
        assert_eq!(lib.get(&one.id), Some(&one));

        // Once the file is gone from the directory the game goes.
        fs::remove_file(&rom).unwrap();
        let report = lib.register_bundled(&dir).unwrap();
        assert_eq!((report.skipped, report.removed), (0, 1));
        assert!(lib.get(&one.id).is_none());
        assert_eq!(lib.entries().len(), 1);
    }

    #[test]
    fn an_unchanged_bundled_rom_is_not_read_again() {
        let f = fixture();
        let dir = bundle_dir(&f);
        let rom = write_rom(&dir, "one.gb", &gb_rom("ONE", 0));
        let mut lib = Library::open(&f.data);
        lib.register_bundled(&dir).unwrap();
        let snapshot = lib.entries().to_vec();
        assert!(snapshot[0].stamp.is_some());

        // Different content of the same size and mtime: a file that was read again
        // would be a different game (added and removed); one that was not, is still ONE.
        let mtime = mtime_of(&rom);
        fs::write(&rom, gb_rom("TWO", 0)).unwrap();
        set_mtime(&rom, mtime);
        assert_eq!(
            lib.register_bundled(&dir).unwrap(),
            BundledReport::default()
        );
        assert_eq!(lib.entries(), snapshot);
        // The stamp is in the index: a new process skips the read too.
        let mut again = Library::open(&f.data);
        assert_eq!(again.entries(), snapshot);
        assert_eq!(
            again.register_bundled(&dir).unwrap(),
            BundledReport::default()
        );
        assert_eq!(again.entries(), snapshot);
    }

    #[test]
    fn a_bundled_rom_with_a_new_mtime_or_size_is_read_again() {
        let f = fixture();
        let dir = bundle_dir(&f);
        let rom = write_rom(&dir, "one.gb", &gb_rom("ONE", 0));
        let mut lib = Library::open(&f.data);
        lib.register_bundled(&dir).unwrap();
        let mtime = mtime_of(&rom);

        // Same content, newer file: read again, nothing changes, the new stamp is kept.
        set_mtime(&rom, mtime + Duration::from_secs(60));
        let before = lib.entries()[0].clone();
        assert_eq!(
            lib.register_bundled(&dir).unwrap(),
            BundledReport::default()
        );
        assert_ne!(lib.entries()[0].stamp, before.stamp);
        assert_eq!(Library::open(&f.data).entries(), lib.entries());

        // A change of size is noticed too.
        fs::write(&rom, b"junk").unwrap();
        let report = lib.register_bundled(&dir).unwrap();
        assert_eq!(report.skipped, 1);
    }

    #[test]
    fn an_index_without_stamps_still_loads_and_gets_them() {
        let f = fixture();
        let dir = bundle_dir(&f);
        write_rom(&dir, "one.gb", &gb_rom("ONE", 0));
        let mut lib = Library::open(&f.data);
        lib.register_bundled(&dir).unwrap();
        let text = fs::read_to_string(f.data.index_file()).unwrap();
        let mut index: serde_json::Value = serde_json::from_str(&text).unwrap();
        for game in index["games"].as_array_mut().unwrap() {
            game.as_object_mut().unwrap().remove("rom_size");
            game.as_object_mut().unwrap().remove("rom_mtime_ns");
        }
        fs::write(f.data.index_file(), index.to_string()).unwrap();
        let mut old = Library::open(&f.data);
        assert_eq!(old.entries().len(), 1);
        assert!(old.entries()[0].stamp.is_none());
        let report = old.register_bundled(&dir).unwrap();
        assert_eq!(report, BundledReport::default());
        assert!(old.entries()[0].stamp.is_some());
        assert!(Library::open(&f.data).entries()[0].stamp.is_some());
    }

    #[test]
    fn a_bundled_game_from_another_folder_is_unlisted_even_if_its_file_exists() {
        let f = fixture();
        let dev = f.roms.parent().unwrap().join("roms-homebrew");
        fs::create_dir_all(&dev).unwrap();
        write_rom(&dev, "one.gb", &gb_rom("ONE", 0));
        let packaged = bundle_dir(&f);
        write_rom(&packaged, "two.gb", &gb_rom("TWO", 0));
        let mut lib = Library::open(&f.data);
        lib.register_bundled(&dev).unwrap();
        let one = lib.entries()[0].id.clone();
        fs::write(lib.paths_for(&one).sav, b"progress").unwrap();

        let report = lib.register_bundled(&packaged).unwrap();
        assert_eq!((report.added, report.removed), (1, 1));
        assert!(dev.join("one.gb").is_file());
        assert!(lib.get(&one).is_none());
        assert_eq!(lib.entries().len(), 1);
        assert_eq!(fs::read(lib.paths_for(&one).sav).unwrap(), b"progress");
    }

    #[test]
    fn a_bundled_game_replaced_in_place_by_another_one_is_swapped() {
        let f = fixture();
        let dir = bundle_dir(&f);
        let rom = write_rom(&dir, "one.gb", &gb_rom("ONE", 0));
        let mut lib = Library::open(&f.data);
        lib.register_bundled(&dir).unwrap();
        let old = lib.entries()[0].id.clone();
        fs::write(&rom, gb_rom("ONE", 9)).unwrap();
        // Same size: only the time tells the file changed (it can tie on a coarse clock).
        set_mtime(&rom, SystemTime::now() + Duration::from_secs(60));
        let report = lib.register_bundled(&dir).unwrap();
        assert_eq!((report.added, report.removed), (1, 1));
        assert!(lib.get(&old).is_none());
    }

    #[test]
    fn a_thumbnail_can_be_set_for_a_game_another_instance_added() {
        let f = fixture();
        let mut first = Library::open(&f.data);
        let mut second = Library::open(&f.data);
        let e = second
            .add_rom(&write_rom(&f.roms, "a.gb", &gb_rom("MOTH", 0)))
            .unwrap()
            .entry;
        // `first` was opened before the game existed; it re-reads the index.
        assert!(first.get(&e.id).is_none());
        let path = first.set_thumbnail(&e.id, &tiny_png()).unwrap();
        assert!(path.is_file());
        assert!(first.get(&e.id).unwrap().thumbnail.is_some());
        // An id nobody has is still refused, and nothing is written for it.
        let nobody = GameId::new("nobody", 7);
        assert!(first.set_thumbnail(&nobody, &tiny_png()).is_err());
        assert!(!thumbnail_path(&f.data, &nobody).exists());
    }

    #[test]
    fn bundled_games_cannot_be_removed_and_a_players_copy_stays_theirs() {
        let f = fixture();
        let dir = bundle_dir(&f);
        write_rom(&dir, "one.gb", &gb_rom("ONE", 0));
        let mut lib = Library::open(&f.data);
        let mine = lib
            .add_rom(&write_rom(&f.roms, "mine.gb", &gb_rom("ONE", 0)))
            .unwrap()
            .entry;
        lib.register_bundled(&dir).unwrap();
        assert_eq!(lib.entries().len(), 1);
        assert_eq!(lib.get(&mine.id).unwrap().source, Source::Imported);

        write_rom(&dir, "two.gb", &gb_rom("TWO", 0));
        lib.register_bundled(&dir).unwrap();
        let two = lib
            .entries()
            .iter()
            .find(|e| e.title == "TWO")
            .unwrap()
            .id
            .clone();
        let err = lib.remove(&two, RemoveSaves::Delete).unwrap_err();
        assert!(err.contains("bundled"), "{err}");
        assert!(dir.join("two.gb").exists());
        assert!(lib.get(&two).is_some());
    }

    #[test]
    fn touch_played_is_saved() {
        let f = fixture();
        let mut lib = Library::open(&f.data);
        let e = lib
            .add_rom(&write_rom(&f.roms, "a.gb", &gb_rom("MOTH", 0)))
            .unwrap()
            .entry;
        lib.touch_played(&e.id).unwrap();
        let played = Library::open(&f.data)
            .get(&e.id)
            .unwrap()
            .last_played
            .unwrap();
        assert!(played >= e.added);
        let missing = GameId::new("nothing", 1);
        assert!(lib.touch_played(&missing).is_err());
    }

    fn tiny_png() -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 4, 2);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[0, 50, 100, 150, 200, 250, 1, 2])
            .unwrap();
        drop(w);
        out
    }

    #[test]
    fn thumbnails_are_checked_stored_and_read_back() {
        let f = fixture();
        let mut lib = Library::open(&f.data);
        let e = lib
            .add_rom(&write_rom(&f.roms, "a.gb", &gb_rom("MOTH", 0)))
            .unwrap()
            .entry;
        assert_eq!(lib.read_thumbnail(&e.id), None);
        let png = tiny_png();
        let path = lib.set_thumbnail(&e.id, &png).unwrap();
        assert_eq!(path, f.data.games().join(format!("{}.png", e.id)));
        assert_eq!(lib.get(&e.id).unwrap().thumbnail, Some(path.clone()));
        assert_eq!(lib.read_thumbnail(&e.id), Some(png));
        // It is found again after a restart.
        assert_eq!(
            Library::open(&f.data).get(&e.id).unwrap().thumbnail,
            Some(path)
        );

        assert!(lib.set_thumbnail(&e.id, b"not a png").is_err());
        assert!(lib.set_thumbnail(&e.id, &tiny_png()[..30]).is_err());
        assert!(
            lib.set_thumbnail(&GameId::new("x", 1), &tiny_png())
                .is_err()
        );
        let mut huge = Vec::new();
        let mut enc = png::Encoder::new(&mut huge, MAX_THUMBNAIL_SIDE + 1, 1);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&vec![0u8; MAX_THUMBNAIL_SIDE as usize + 1])
            .unwrap();
        drop(w);
        assert!(
            lib.set_thumbnail(&e.id, &huge)
                .unwrap_err()
                .contains("at most")
        );
        // A rejected thumbnail leaves the good one in place.
        assert!(lib.read_thumbnail(&e.id).is_some());
    }

    #[test]
    fn a_save_beside_the_rom_is_copied_in_once_and_the_original_left_alone() {
        let f = fixture();
        let rom = write_rom(&f.roms, "moth.gb", &gb_rom("MOTH", 0));
        fs::write(f.roms.join("moth.sav"), b"old save").unwrap();
        fs::write(f.roms.join("moth.ss1"), b"state one").unwrap();
        fs::write(f.roms.join("moth.ss9"), b"state nine").unwrap();
        let id = GameId::new("MOTH", rom_hash(&fs::read(&rom).unwrap()));
        let paths = GamePaths::new(&f.data, &id);

        let copied = import_beside_rom(&rom, &paths);
        assert_eq!(copied.len(), 1, "the save is copied");
        assert_eq!(fs::read(&paths.sav).unwrap(), b"old save");
        // The old slots become snapshots; nothing is left in their old place.
        let snapshots = crate::state::StateStore::new(paths.clone()).snapshots();
        assert_eq!(snapshots.len(), 2);
        let contents: Vec<Vec<u8>> = snapshots
            .iter()
            .map(|s| fs::read(paths.snapshot_state(s.secs)).unwrap())
            .collect();
        assert!(contents.contains(&b"state one".to_vec()));
        assert!(contents.contains(&b"state nine".to_vec()));
        assert!(!paths.legacy_state(1).exists());
        assert_eq!(fs::read(f.roms.join("moth.sav")).unwrap(), b"old save");
        assert!(f.roms.join("moth.ss1").exists());

        // The player goes on; the next open must not bring the old file back.
        fs::write(&paths.sav, b"new progress").unwrap();
        fs::write(f.roms.join("moth.sav"), b"changed beside the rom").unwrap();
        assert!(import_beside_rom(&rom, &paths).is_empty());
        assert_eq!(fs::read(&paths.sav).unwrap(), b"new progress");
        assert_eq!(
            crate::state::StateStore::new(paths).snapshots().len(),
            2,
            "the slots are not imported again"
        );
    }

    #[test]
    fn importing_ignores_what_is_not_a_usable_file() {
        let f = fixture();
        let rom = f.roms.join("moth.gb");
        fs::create_dir(f.roms.join("moth.sav")).unwrap();
        fs::write(f.roms.join("moth.ss2"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        let paths = GamePaths::new(&f.data, &GameId::new("MOTH", 1));
        assert!(import_beside_rom(&rom, &paths).is_empty());
        assert!(!paths.sav.exists());
        assert!(crate::state::StateStore::new(paths).snapshots().is_empty());
    }

    #[test]
    fn opening_a_rom_directly_uses_app_folder_paths_and_imports() {
        let f = fixture();
        let rom = write_rom(&f.roms, "moth.gb", &gb_rom("MOTH", 0));
        fs::write(f.roms.join("moth.sav"), b"old").unwrap();
        let hash = rom_hash(&fs::read(&rom).unwrap());
        let paths = paths_for_opened_rom(Some(&f.data), &rom, "MOTH", hash);
        assert!(paths.sav.starts_with(f.data.saves()));
        assert_eq!(fs::read(&paths.sav).unwrap(), b"old");
        // Moving the ROM keeps the same files.
        let moved = write_rom(&f.roms, "elsewhere.gb", &fs::read(&rom).unwrap());
        assert_eq!(
            paths_for_opened_rom(Some(&f.data), &moved, "MOTH", hash),
            paths
        );
        // And the library gives a ROM added later the same ones.
        let mut lib = Library::open(&f.data);
        let e = lib.add_rom(&moved).unwrap().entry;
        assert_eq!(lib.paths_for(&e.id), paths);
        assert_eq!(lib.entries().len(), 1);
    }

    #[test]
    fn with_no_app_folder_the_files_stay_beside_the_rom() {
        let f = fixture();
        let rom = write_rom(&f.roms, "moth.gb", &gb_rom("MOTH", 0));
        fs::write(f.roms.join("moth.sav"), b"old").unwrap();
        let paths = paths_for_opened_rom(None, &rom, "MOTH", 1);
        assert_eq!(paths.sav, f.roms.join("moth.sav"));
        assert_eq!(paths.legacy_state(3), f.roms.join("moth.ss3"));
        assert_eq!(paths.states_dir(), f.roms.join("moth.states"));
        assert!(paths.screenshot(1_700_000_000, 7).starts_with(&f.roms));
        assert_eq!(
            fs::read(&paths.sav).unwrap(),
            b"old",
            "nothing is copied or changed"
        );
        assert_eq!(fs::read_dir(&f.roms).unwrap().count(), 2);
    }

    #[test]
    fn the_beside_layout_never_names_the_rom_itself() {
        for name in [
            "g.sav", "g.ss1", "g.SS9", "g.ss5", "g.states", "noext", "g.gb",
        ] {
            let rom = PathBuf::from("d").join(name);
            let p = GamePaths::beside(&rom);
            let mut all = vec![p.sav.clone(), p.screenshot(0, 1)];
            all.extend((1..=9).map(|n| p.legacy_state(n)));
            all.extend([p.states_dir().to_owned(), p.resume_state()]);
            assert!(all.iter().all(|f| *f != rom), "{name}: {all:?}");
        }
    }

    #[test]
    fn add_imports_the_saves_that_sit_beside_the_source_rom() {
        let f = fixture();
        let src = write_rom(&f.roms, "moth.gb", &gb_rom("MOTH", 0));
        fs::write(f.roms.join("moth.sav"), b"old").unwrap();
        let mut lib = Library::open(&f.data);
        let e = lib.add_rom(&src).unwrap().entry;
        assert_eq!(fs::read(lib.paths_for(&e.id).sav).unwrap(), b"old");
        assert_eq!(fs::read(f.roms.join("moth.sav")).unwrap(), b"old");
    }

    #[test]
    fn screenshot_names_count_frames_in_seven_digits() {
        let p = GamePaths::beside(Path::new("shots/Red.gb"));
        let at = 1_700_000_000;
        assert_eq!(
            p.screenshot(at, 1234),
            PathBuf::from("shots/Red-20231114-221320-f0001234.png")
        );
        assert_eq!(
            p.screenshot(at, 5),
            PathBuf::from("shots/Red-20231114-221320-f0000005.png")
        );
        // The same frame in a later session does not reuse the name.
        assert_ne!(p.screenshot(at, 5), p.screenshot(at + 60, 5));
        assert!(!is_screenshot_suffix("f0000005.png"));
        assert!(is_screenshot_suffix("20231114-221320-f0000005.png"));
        assert_eq!(p.sav, PathBuf::from("shots/Red.sav"));
        assert_eq!(p.legacy_state(2), PathBuf::from("shots/Red.ss2"));
    }
}
