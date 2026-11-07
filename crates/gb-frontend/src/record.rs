//! `--record`: append the session's inputs to a replay file, and screenshots.
//!
//! Records are written straight to the file as they happen (a few per second
//! while playing), so a crash loses at most the final `End` record;
//! `gb-test --replay` still plays such a file up to its last input.
//!
//! Recordings are system-aware (format v3 and v4): the header carries the system
//! byte, so a GBA session records its L and R buttons and its battery image (at
//! most 128 KiB of Flash, inside the codec's 256 KiB bound), and, from the core's
//! `recording_ext`, what else powers the machine on the way the session did (a GBA's
//! cartridge options, BIOS identity and clock start: the ROM and battery alone
//! do not reproduce a GBA run).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use emu_common::replay::{self, Header, Record};
use emu_common::{Core, FrameRef, Pixels, rgb555_to_rgb888};

/// The header of a recording of `core`'s session: call it right after the save
/// was loaded, so the battery bytes are what the session really powered on with.
pub fn header_for(core: &dyn Core, rom_hash: u64) -> Header {
    Header {
        system: core.system(),
        rom_hash,
        battery: core.battery_ram(),
        variant: core.hw_variant(),
        ext: core.recording_ext(),
    }
}

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
        file.write_all(&header.encode())
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
            log::warn!(
                "recording stopped, cannot write {}: {e}",
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
            log::warn!(
                "recording {} is incomplete (it still replays up to the failure)",
                self.path.display()
            );
            return;
        }
        match self.file.sync_all() {
            Ok(()) => log::info!("recording saved: {} ({frames} frames)", self.path.display()),
            Err(e) => log::warn!("cannot flush recording {}: {e}", self.path.display()),
        }
    }
}

/// Write the raw frame as a PNG at its own size: greyscale for shades, which
/// map exactly as `gb-test` dumps them (0xFF, 0xAA, 0x55, 0x00), and RGB for an
/// RGB555 frame, widened with no colour correction (as `video::blit_rgb555` and
/// `gb-test` do). A screenshot and the replayed frame compare byte for byte.
pub fn write_png(frame: FrameRef<'_>, path: &Path) -> Result<(), String> {
    let width = u32::try_from(frame.width).map_err(|e| e.to_string())?;
    let height = u32::try_from(frame.height).map_err(|e| e.to_string())?;
    let file = File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    enc.set_depth(png::BitDepth::Eight);
    let samples: Vec<u8> = match frame.pixels {
        Pixels::Shades(fb) => {
            enc.set_color(png::ColorType::Grayscale);
            fb.iter().map(|&s| 0xFF - 0x55 * (s & 3)).collect()
        }
        Pixels::Rgb555(fb) => {
            enc.set_color(png::ColorType::Rgb);
            fb.iter().flat_map(|&c| rgb555_to_rgb888(c)).collect()
        }
    };
    enc.write_header()
        .and_then(|mut w| w.write_image_data(&samples))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_has_the_frames_own_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.png");
        let px: Vec<u16> = (0..240 * 160).map(|i| i as u16 & 0x7FFF).collect();
        let frame = FrameRef {
            width: 240,
            height: 160,
            pixels: Pixels::Rgb555(&px),
        };
        write_png(frame, &path).unwrap();
        let dec = png::Decoder::new(std::io::BufReader::new(File::open(&path).unwrap()));
        let reader = dec.read_info().unwrap();
        assert_eq!((reader.info().width, reader.info().height), (240, 160));
    }

    #[test]
    fn refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.gbinput");
        std::fs::write(&path, b"keep").unwrap();
        let header = Header {
            system: emu_common::System::Gb,
            rom_hash: 0,
            battery: None,
            variant: 0,
            ext: Vec::new(),
        };
        assert!(Recorder::create(&path, &header).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    }
}
