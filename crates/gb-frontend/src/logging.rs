//! The `log` facade backend: every record goes to a size-rotated file in the
//! app folder, and records from this program also go to stderr exactly as
//! `eprintln!` printed them before (no timestamp, `warning: ` / `error: `
//! prefixes), so a terminal user sees the same output.
//!
//! The file is `gameboyem.log`; when it would pass [`MAX_LOG_BYTES`] it becomes
//! `gameboyem.log.1`, the previous `.1` becomes `.2`, and the oldest goes, so at
//! most [`LOG_FILES`] files exist. Lines are `2026-11-07T19:03:05.123Z INFO  text`
//! (UTC, marked with `Z`) and are written as they happen, not buffered, so a
//! crash loses nothing. A file that cannot be written is dropped with one
//! warning and logging goes on to stderr.

#![allow(dead_code, reason = "wired into main in the next commit")]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};

/// Largest size a log file grows to before it is rotated (a record is never split).
pub const MAX_LOG_BYTES: u64 = 1024 * 1024;
/// Log files kept: the current one and the older ones.
pub const LOG_FILES: usize = 3;
/// Longest record written; the rest of a longer message is cut off.
const MAX_RECORD_BYTES: usize = 16 * 1024;

/// Environment variable that sets the level: `error`, `warn`, `info` (default),
/// `debug` or `trace`.
pub const ENV_LOG_LEVEL: &str = "GAMEBOYEM_LOG";

/// An append-only text file that rotates by size.
pub struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    files: usize,
    /// `None` after a failed rotation until the next write reopens it.
    file: Option<File>,
    size: u64,
}

impl RotatingFile {
    /// Open (or create) `path` for appending. `files` is how many files exist at
    /// most, counting the current one; `max_bytes` is each one's cap. A file
    /// that is already full is rotated at once.
    pub fn open(path: &Path, max_bytes: u64, files: usize) -> io::Result<Self> {
        let file = open_append(path)?;
        let size = file.metadata()?.len();
        let mut me = Self {
            path: path.to_owned(),
            max_bytes,
            files: files.max(1),
            file: Some(file),
            size,
        };
        if me.size >= me.max_bytes {
            me.rotate()?;
        }
        Ok(me)
    }

    /// Append `text` and a newline, rotating first if it would not fit.
    pub fn write_record(&mut self, text: &str) -> io::Result<()> {
        let text = truncate_at_boundary(text, MAX_RECORD_BYTES);
        let len = text.len() as u64 + 1;
        if self.size > 0 && self.size + len > self.max_bytes {
            self.rotate()?;
        }
        if self.file.is_none() {
            let file = open_append(&self.path)?;
            self.size = file.metadata()?.len();
            self.file = Some(file);
        }
        let mut line = Vec::with_capacity(text.len() + 1);
        line.extend_from_slice(text.as_bytes());
        line.push(b'\n');
        if let Some(file) = self.file.as_mut() {
            file.write_all(&line)?;
        }
        self.size += len;
        Ok(())
    }

    fn numbered(&self, n: usize) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    /// `log` -> `log.1` -> `log.2` ..., dropping the oldest, then start a new `log`.
    fn rotate(&mut self) -> io::Result<()> {
        // Close first: Windows will not rename an open file.
        self.file = None;
        if self.files == 1 {
            remove_if_exists(&self.path)?;
        } else {
            remove_if_exists(&self.numbered(self.files - 1))?;
            for n in (1..self.files - 1).rev() {
                rename_if_exists(&self.numbered(n), &self.numbered(n + 1))?;
            }
            rename_if_exists(&self.path, &self.numbered(1))?;
        }
        self.file = Some(open_append(&self.path)?);
        self.size = 0;
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn rename_if_exists(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn truncate_at_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `1700000000.5s` as `2023-11-14T22:13:20.500Z`.
fn utc_timestamp(since_epoch: Duration) -> String {
    let secs = since_epoch.as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let day_secs = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        day_secs / 3600,
        day_secs % 3600 / 60,
        day_secs % 60,
        since_epoch.subsec_millis()
    )
}

/// Days since 1970-01-01 to a calendar date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Whether `target` (the module path of the macro call) is this program or its
/// cores, as opposed to a dependency such as the graphics stack.
fn is_ours(target: &str) -> bool {
    target == "gb"
        || target.starts_with("gb::")
        || target.starts_with("gb_")
        || target.starts_with("gba_")
        || target.starts_with("emu_common")
}

/// What stderr shows for a record: the message as `eprintln!` printed it before.
fn stderr_text(level: Level, message: &str) -> String {
    match level {
        Level::Error => format!("error: {message}"),
        Level::Warn => format!("warning: {message}"),
        _ => message.to_owned(),
    }
}

/// The level named by `GAMEBOYEM_LOG`, `info` if unset or not a level.
pub fn level_from_env() -> LevelFilter {
    std::env::var(ENV_LOG_LEVEL)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(LevelFilter::Info)
}

struct AppLogger {
    level: LevelFilter,
    file: Mutex<Option<RotatingFile>>,
}

impl AppLogger {
    fn new(level: LevelFilter, file: Option<RotatingFile>) -> Self {
        Self {
            level,
            file: Mutex::new(file),
        }
    }
}

impl Log for AppLogger {
    /// This program's records at `level` and below; a dependency's only if they
    /// are warnings or errors.
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
            && (is_ours(metadata.target()) || metadata.level() <= Level::Warn)
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = record.args().to_string();
        if is_ours(record.target()) {
            let _ = writeln!(
                io::stderr().lock(),
                "{}",
                stderr_text(record.level(), &message)
            );
        }
        // A poisoned lock only means another thread panicked mid-write.
        let mut guard = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(file) = guard.as_mut() {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let line = format!(
                "{} {:<5} {message}",
                utc_timestamp(now),
                record.level().as_str()
            );
            if let Err(e) = file.write_record(&line) {
                let _ = writeln!(
                    io::stderr().lock(),
                    "warning: cannot write the log file, logging to stderr only: {e}"
                );
                *guard = None;
            }
        }
    }

    fn flush(&self) {}
}

/// Install the logger. `file` is the log file path; `None`, or a file that
/// cannot be opened (reported once), logs to stderr only. Call once, early:
/// records logged before this are dropped.
pub fn init(file: Option<&Path>, level: LevelFilter) {
    let file = file.and_then(|path| {
        match RotatingFile::open(path, MAX_LOG_BYTES, LOG_FILES) {
            Ok(f) => Some(f),
            Err(e) => {
                let _ = writeln!(
                    io::stderr().lock(),
                    "warning: cannot open the log file {}: {e}",
                    path.display()
                );
                None
            }
        }
    });
    if log::set_boxed_logger(Box::new(AppLogger::new(level, file))).is_err() {
        return;
    }
    log::set_max_level(level);
    // A panic goes to the file too. Its target is not this program's, so it is
    // not repeated on stderr, where the default hook prints it.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!(target: "panic", "{info}");
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    fn rotated(dir: &Path, n: usize) -> PathBuf {
        dir.join(format!("t.log.{n}"))
    }

    #[test]
    fn timestamps_are_utc_calendar_dates() {
        let t = |s: u64, ms: u32| utc_timestamp(Duration::new(s, ms * 1_000_000));
        assert_eq!(t(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(t(1_700_000_000, 500), "2023-11-14T22:13:20.500Z");
        assert_eq!(t(951_782_400, 0), "2000-02-29T00:00:00.000Z");
        assert_eq!(t(4_102_444_800, 7), "2100-01-01T00:00:00.007Z");
    }

    #[test]
    fn a_record_is_appended_with_a_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.log");
        let mut f = RotatingFile::open(&path, 1000, 3).unwrap();
        f.write_record("one").unwrap();
        f.write_record("two").unwrap();
        assert_eq!(read(&path), "one\ntwo\n");
        // Reopening appends.
        let mut f = RotatingFile::open(&path, 1000, 3).unwrap();
        f.write_record("three").unwrap();
        assert_eq!(read(&path), "one\ntwo\nthree\n");
    }

    #[test]
    fn a_full_file_rotates_and_only_the_last_three_files_stay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.log");
        // Each record is 10 bytes with its newline; the cap fits two of them.
        let mut f = RotatingFile::open(&path, 25, 3).unwrap();
        for i in 0..8 {
            f.write_record(&format!("record-{i:02}")).unwrap();
        }
        // 8 records, 2 per file: record-06/07 current, 04/05 in .1, 02/03 in .2,
        // and the first two files are gone.
        assert_eq!(read(&path), "record-06\nrecord-07\n");
        assert_eq!(read(&rotated(dir.path(), 1)), "record-04\nrecord-05\n");
        assert_eq!(read(&rotated(dir.path(), 2)), "record-02\nrecord-03\n");
        assert!(!rotated(dir.path(), 3).exists());
        for entry in fs::read_dir(dir.path()).unwrap() {
            assert!(entry.unwrap().metadata().unwrap().len() <= 25);
        }
    }

    #[test]
    fn a_file_that_is_already_full_rotates_when_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.log");
        fs::write(&path, "x".repeat(40)).unwrap();
        let mut f = RotatingFile::open(&path, 40, 3).unwrap();
        f.write_record("fresh").unwrap();
        assert_eq!(read(&path), "fresh\n");
        assert_eq!(read(&rotated(dir.path(), 1)), "x".repeat(40));
    }

    #[test]
    fn a_single_file_budget_just_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.log");
        let mut f = RotatingFile::open(&path, 12, 1).unwrap();
        f.write_record("aaaaaaaaa").unwrap();
        f.write_record("bbbbbbbbb").unwrap();
        assert_eq!(read(&path), "bbbbbbbbb\n");
        assert!(!rotated(dir.path(), 1).exists());
    }

    #[test]
    fn a_huge_record_is_cut_and_still_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.log");
        let mut f = RotatingFile::open(&path, 1 << 20, 3).unwrap();
        f.write_record(&"é".repeat(MAX_RECORD_BYTES)).unwrap();
        let text = read(&path);
        assert!(text.len() <= MAX_RECORD_BYTES + 1);
        assert!(text.ends_with("é\n"));
    }

    #[test]
    fn stderr_keeps_the_old_wording() {
        assert_eq!(stderr_text(Level::Warn, "cannot x"), "warning: cannot x");
        assert_eq!(stderr_text(Level::Error, "boom"), "error: boom");
        assert_eq!(stderr_text(Level::Info, "loaded save"), "loaded save");
    }

    #[test]
    fn dependencies_are_logged_only_from_warnings_up() {
        let logger = AppLogger::new(LevelFilter::Info, None);
        let meta = |level, target| Metadata::builder().level(level).target(target).build();
        assert!(logger.enabled(&meta(Level::Info, "gb::save")));
        assert!(logger.enabled(&meta(Level::Info, "gb_core::cpu")));
        assert!(!logger.enabled(&meta(Level::Debug, "gb::save")));
        assert!(!logger.enabled(&meta(Level::Info, "wgpu_hal::metal")));
        assert!(logger.enabled(&meta(Level::Warn, "wgpu_hal::metal")));
    }

    #[test]
    fn the_logger_writes_a_timestamped_line_per_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.log");
        let file = RotatingFile::open(&path, 1 << 20, 3).unwrap();
        let logger = AppLogger::new(LevelFilter::Info, Some(file));
        logger.log(
            &Record::builder()
                .level(Level::Warn)
                .target("wgpu_core")
                .args(format_args!("first"))
                .build(),
        );
        logger.log(
            &Record::builder()
                .level(Level::Debug)
                .target("wgpu_core")
                .args(format_args!("dropped"))
                .build(),
        );
        let text = read(&path);
        let line = text.lines().next().unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(line.ends_with("Z WARN  first"), "{line}");
        assert_eq!(line.as_bytes()[4], b'-');
    }
}
