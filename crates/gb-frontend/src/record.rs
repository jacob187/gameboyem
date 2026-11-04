//! `--record`: append the session's inputs to a replay file, and screenshots.
//!
//! Records are written straight to the file as they happen (a few per second
//! while playing), so a crash loses at most the final `End` record;
//! `gb-test --replay` still plays such a file up to its last input.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use gb_core::replay::{self, Header, Record};
use gb_core::{Framebuffer, SCREEN_HEIGHT, SCREEN_WIDTH};

pub struct Recorder {
    file: File,
    path: PathBuf,
    failed: bool,
}

impl Recorder {
    /// Start a new recording. Refuses to overwrite an existing file.
    pub fn create(path: &Path, header: &Header) -> Result<Self, String> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| format!("cannot create recording {}: {e}", path.display()))?;
        file.write_all(&replay::encode_header(header))
            .map_err(|e| format!("cannot write recording {}: {e}", path.display()))?;
        Ok(Self {
            file,
            path: path.to_owned(),
            failed: false,
        })
    }

    pub fn record(&mut self, record: Record) {
        if self.failed {
            return;
        }
        if let Err(e) = self.file.write_all(&replay::encode_record(record)) {
            self.failed = true;
            eprintln!(
                "warning: recording stopped, cannot write {}: {e}",
                self.path.display()
            );
        }
    }

    /// Write the `End` record (total frames) and flush to disk.
    pub fn finish(&mut self, frames: u64) {
        self.record(Record {
            frame: frames,
            event: replay::Event::End,
        });
        if self.failed {
            eprintln!(
                "recording {} is incomplete (it still replays up to the failure)",
                self.path.display()
            );
            return;
        }
        match self.file.sync_all() {
            Ok(()) => eprintln!("recording saved: {} ({frames} frames)", self.path.display()),
            Err(e) => eprintln!(
                "warning: cannot flush recording {}: {e}",
                self.path.display()
            ),
        }
    }
}

/// `dir/game.gb` + frame 1234 -> `dir/game-f0001234.png`.
pub fn screenshot_path(rom: &Path, frame: u64) -> PathBuf {
    let stem = rom.file_stem().unwrap_or(rom.as_os_str()).to_string_lossy();
    rom.with_file_name(format!("{stem}-f{frame:07}.png"))
}

/// Write the raw 160x144 picture as a greyscale PNG. Shades map to grey
/// exactly as `gb-test` dumps them (0xFF, 0xAA, 0x55, 0x00), so a screenshot
/// and the replayed frame can be compared byte for byte.
pub fn write_png(fb: &Framebuffer, path: &Path) -> Result<(), String> {
    let file = File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(
        std::io::BufWriter::new(file),
        SCREEN_WIDTH as u32,
        SCREEN_HEIGHT as u32,
    );
    enc.set_color(png::ColorType::Grayscale);
    enc.set_depth(png::BitDepth::Eight);
    let grey: Vec<u8> = fb.iter().map(|&s| 0xFF - 0x55 * (s & 3)).collect();
    enc.write_header()
        .and_then(|mut w| w.write_image_data(&grey))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_names() {
        assert_eq!(
            screenshot_path(Path::new("games/Red.gb"), 1234),
            PathBuf::from("games/Red-f0001234.png")
        );
        assert_eq!(
            screenshot_path(Path::new("noext"), 5),
            PathBuf::from("noext-f0000005.png")
        );
    }

    #[test]
    fn refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.gbinput");
        std::fs::write(&path, b"keep").unwrap();
        let header = Header {
            rom_hash: 0,
            battery: None,
        };
        assert!(Recorder::create(&path, &header).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    }
}
