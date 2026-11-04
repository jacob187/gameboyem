//! Reading the ROM file and turning load failures into messages a person can
//! act on. Everything here is pure or touches only the filesystem.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use gb_core::CartridgeError;
use gb_core::cartridge::MAX_ROM_SIZE;

/// Reading stops one byte past the core's ROM limit (so the core still reports
/// `TooLarge`), and a bogus huge file or device node cannot exhaust memory.
const READ_LIMIT: u64 = MAX_ROM_SIZE as u64 + 1;

/// Read the ROM at `path`. The error is a ready-to-print message.
pub fn read_rom(path: &Path) -> Result<Vec<u8>, String> {
    read_rom_io(path).map_err(|e| io_message(path, &e))
}

fn read_rom_io(path: &Path) -> io::Result<Vec<u8>> {
    // Opening a directory succeeds on Unix and only fails on read; check up front
    // so both platforms give the same message.
    if fs::metadata(path)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "is a directory",
        ));
    }
    let mut data = Vec::new();
    File::open(path)?.take(READ_LIMIT).read_to_end(&mut data)?;
    Ok(data)
}

/// Message for failing to read the ROM file.
pub fn io_message(path: &Path, e: &io::Error) -> String {
    let what = match e.kind() {
        io::ErrorKind::NotFound => "file not found".to_owned(),
        io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        io::ErrorKind::IsADirectory => "that is a directory, not a ROM file".to_owned(),
        _ => e.to_string(),
    };
    format!("cannot read {}: {what}", path.display())
}

/// Common container formats people mistake for ROMs, from the file's magic bytes.
pub fn archive_kind(data: &[u8]) -> Option<&'static str> {
    const MAGICS: [(&[u8], &str); 4] = [
        (b"PK\x03\x04", "zip"),
        (&[0x1F, 0x8B], "gzip"),
        (b"7z\xBC\xAF\x27\x1C", "7z"),
        (b"Rar!\x1A\x07", "rar"),
    ];
    MAGICS
        .iter()
        .find(|(magic, _)| data.starts_with(magic))
        .map(|&(_, name)| name)
}

/// Human name for cartridge types the core does not implement.
fn cartridge_type_name(t: u8) -> Option<&'static str> {
    Some(match t {
        0x05 | 0x06 => "MBC2",
        0x0B..=0x0D => "MMM01",
        0x20 => "MBC6",
        0x22 => "MBC7",
        0xFC => "Pocket Camera",
        0xFD => "TAMA5",
        0xFE => "HuC3",
        0xFF => "HuC1",
        _ => return None,
    })
}

/// Message for a ROM the core refused. `archive` is [`archive_kind`] of the
/// file contents.
pub fn cartridge_message(path: &Path, e: &CartridgeError, archive: Option<&str>) -> String {
    let reason = match e {
        CartridgeError::TooSmall(0) => "the file is empty".to_owned(),
        CartridgeError::TooSmall(n) => {
            format!("the file is only {n} bytes; a Game Boy ROM is at least 336 (0x150) bytes")
        }
        // `n` is capped by the reader, so the real size is unknown.
        CartridgeError::TooLarge(_) => {
            "the file is larger than 8 MiB, the biggest ROM supported".to_owned()
        }
        CartridgeError::UnsupportedType(t) => match cartridge_type_name(*t) {
            Some(name) => format!(
                "cartridge type 0x{t:02X} ({name}) is not supported \
                 (supported: ROM-only, MBC1, MBC3, MBC5)"
            ),
            None => format!(
                "cartridge type 0x{t:02X} is not supported or this is not a Game Boy ROM \
                 (supported: ROM-only, MBC1, MBC3, MBC5)"
            ),
        },
        CartridgeError::InvalidRomSize(c) => {
            format!(
                "the header has an invalid ROM size code 0x{c:02X}; the file is probably not a Game Boy ROM"
            )
        }
        CartridgeError::InvalidRamSize(c) => {
            format!(
                "the header has an invalid RAM size code 0x{c:02X}; the file is probably not a Game Boy ROM"
            )
        }
        // Save-related errors cannot come from loading a ROM; keep the core's text.
        CartridgeError::SaveSizeMismatch { .. } | CartridgeError::NoBattery => e.to_string(),
    };
    let hint = archive.map_or(String::new(), |kind| {
        format!(" (this looks like a {kind} archive; extract the ROM first)")
    });
    format!("{} is not a usable ROM: {reason}{hint}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "g.gb";

    fn msg(e: CartridgeError) -> String {
        cartridge_message(Path::new(P), &e, None)
    }

    #[test]
    fn every_cartridge_error_has_a_specific_message() {
        assert!(msg(CartridgeError::TooSmall(0)).contains("empty"));
        let small = msg(CartridgeError::TooSmall(100));
        assert!(small.contains("100 bytes") && small.contains("336"));
        let large = msg(CartridgeError::TooLarge(8 * 1024 * 1024 + 1));
        assert!(large.contains("larger than 8 MiB") && !large.contains("8388609"));
        let mbc2 = msg(CartridgeError::UnsupportedType(0x06));
        assert!(mbc2.contains("0x06") && mbc2.contains("MBC2") && mbc2.contains("MBC5"));
        let odd = msg(CartridgeError::UnsupportedType(0x77));
        assert!(odd.contains("0x77") && odd.contains("not a Game Boy ROM"));
        assert!(msg(CartridgeError::InvalidRomSize(0x52)).contains("ROM size code 0x52"));
        assert!(msg(CartridgeError::InvalidRamSize(0x09)).contains("RAM size code 0x09"));
        assert!(msg(CartridgeError::NoBattery).contains("battery"));
        assert!(
            msg(CartridgeError::SaveSizeMismatch {
                expected: 2,
                actual: 1
            })
            .contains("save file")
        );
        assert!(msg(CartridgeError::TooSmall(0)).starts_with("g.gb is not a usable ROM"));
    }

    #[test]
    fn archives_are_recognised() {
        assert_eq!(archive_kind(b"PK\x03\x04rest"), Some("zip"));
        assert_eq!(archive_kind(&[0x1F, 0x8B, 8]), Some("gzip"));
        assert_eq!(archive_kind(b"7z\xBC\xAF\x27\x1C.."), Some("7z"));
        assert_eq!(archive_kind(b"Rar!\x1A\x07\x00"), Some("rar"));
        assert_eq!(archive_kind(&[0u8; 64]), None);
        assert_eq!(archive_kind(b""), None);
        let hinted = cartridge_message(
            Path::new("g.zip"),
            &CartridgeError::UnsupportedType(0x50),
            Some("zip"),
        );
        assert!(hinted.contains("zip archive; extract"));
    }

    #[test]
    fn read_errors_are_friendly() {
        let dir = tempfile::tempdir().unwrap();

        let missing = read_rom(&dir.path().join("nope.gb")).unwrap_err();
        assert!(missing.contains("file not found") && missing.contains("nope.gb"));

        let is_dir = read_rom(dir.path()).unwrap_err();
        assert!(is_dir.contains("directory"), "{is_dir}");

        let ok = dir.path().join("ok.gb");
        fs::write(&ok, b"abc").unwrap();
        assert_eq!(read_rom(&ok).unwrap(), b"abc");
    }

    #[test]
    fn oversized_files_are_truncated_at_the_limit_plus_one() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.gb");
        fs::write(&big, vec![0u8; MAX_ROM_SIZE + 4096]).unwrap();
        assert_eq!(read_rom(&big).unwrap().len(), MAX_ROM_SIZE + 1);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_reports_permission_denied() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("locked.gb");
        fs::write(&f, b"abc").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o000)).unwrap();
        if File::open(&f).is_err() {
            let m = read_rom(&f).unwrap_err();
            assert!(m.contains("permission denied"), "{m}");
        }
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn real_core_errors_map_to_messages() {
        // Garbage that parses far enough to hit the type check.
        let mut rom = vec![0u8; 0x8000];
        rom[0x147] = 0xFC;
        let e = gb_core::GameBoy::new(rom).err().unwrap();
        assert!(cartridge_message(Path::new(P), &e, None).contains("Pocket Camera"));
        let e = gb_core::GameBoy::new(Vec::new()).err().unwrap();
        assert!(cartridge_message(Path::new(P), &e, None).contains("empty"));
    }
}
