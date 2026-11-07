//! Reading the ROM file and turning load failures into messages a person can
//! act on. Everything here is pure or touches only the filesystem.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use emu_common::System;
use gb_core::CartridgeError;
use gb_core::cartridge::MAX_ROM_SIZE;
use gba_core::CartError;

/// The largest ROM image `system`'s core accepts.
pub fn max_rom_bytes(system: System) -> u64 {
    match system {
        System::Gb => MAX_ROM_SIZE as u64,
        System::Gba => gba_core::MAX_ROM_SIZE as u64,
    }
}

/// Whether `head` (the start of a file) has a GBA cartridge header: the fixed
/// byte 0x96 at 0xB2 and a valid complement checksum at 0xBD (the sum of
/// 0xA0..=0xBC, negated, minus 0x19). A Game Boy header never matches by chance
/// in practice: both bytes must agree.
pub fn looks_like_gba(head: &[u8]) -> bool {
    if head.len() <= 0xBD || head[0xB2] != 0x96 {
        return false;
    }
    let sum = head[0xA0..=0xBC]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_add(b));
    head[0xBD] == 0u8.wrapping_sub(sum).wrapping_sub(0x19)
}

/// The system `path` is for when the flag or the file name settles it without
/// looking inside: `forced` (`--system`), else GBA for a `.gba` name (any case).
fn system_without_reading(path: &Path, forced: Option<System>) -> Option<System> {
    forced.or_else(|| {
        path.extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("gba"))
            .then_some(System::Gba)
    })
}

/// Read the ROM at `path` once and say which system it is for (DECISIONS #46):
/// `--system`, else a `.gba` name, else GBA if the header looks like one, else
/// Game Boy. When the flag or name decides, the file is read with that system's
/// size cap. Otherwise it is read with the largest cap (GBA's), the system is
/// detected from the buffer, and the buffer is then cut to that system's cap + 1
/// byte, so a Game Boy ROM over 8 MiB still reaches the core as `TooLarge`, as
/// before. Archives are not unpacked, so there is no inside-the-archive case: a
/// zip is reported by its magic later.
pub fn load_rom(path: &Path, forced: Option<System>) -> Result<(System, Vec<u8>), String> {
    if let Some(system) = system_without_reading(path, forced) {
        return read_rom(path, system).map(|rom| (system, rom));
    }
    let largest = max_rom_bytes(System::Gb).max(max_rom_bytes(System::Gba));
    let mut rom = read_rom_io(path, largest + 1).map_err(|e| io_message(path, &e))?;
    let system = if looks_like_gba(&rom) {
        System::Gba
    } else {
        System::Gb
    };
    let keep = usize::try_from(max_rom_bytes(system) + 1).unwrap_or(usize::MAX);
    if rom.len() > keep {
        rom.truncate(keep);
        rom.shrink_to_fit();
    }
    Ok((system, rom))
}

/// Read and validate a GBA BIOS dump: exactly [`gba_core::BIOS_SIZE`] bytes.
/// Reading stops one byte past that, so a huge file cannot exhaust memory.
///
/// The official image's hash is not checked: the frontend has no hash crate
/// and the dump cannot be shipped to take an FNV from (DECISIONS #62), so a
/// wrong-but-16 KiB file boots and misbehaves like any other bad BIOS.
pub fn read_bios(path: &Path) -> Result<Box<[u8]>, String> {
    let data =
        read_rom_io(path, gba_core::BIOS_SIZE as u64 + 1).map_err(|e| io_message(path, &e))?;
    check_bios_size(path, data.len())?;
    Ok(data.into_boxed_slice())
}

fn check_bios_size(path: &Path, len: usize) -> Result<(), String> {
    match len.cmp(&gba_core::BIOS_SIZE) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Less => Err(format!(
            "{} is not a GBA BIOS: it is {len} bytes, the BIOS is exactly {} bytes",
            path.display(),
            gba_core::BIOS_SIZE
        )),
        // The reader stopped one byte past the limit, so the real size is unknown.
        std::cmp::Ordering::Greater => Err(format!(
            "{} is not a GBA BIOS: it is larger than {} bytes, the BIOS is exactly that size",
            path.display(),
            gba_core::BIOS_SIZE
        )),
    }
}

/// Read the ROM at `path`. The error is a ready-to-print message. Reading
/// stops one byte past `system`'s ROM limit (so the core still reports
/// `TooLarge`), and a bogus huge file or device node cannot exhaust memory.
pub fn read_rom(path: &Path, system: System) -> Result<Vec<u8>, String> {
    read_rom_io(path, max_rom_bytes(system) + 1).map_err(|e| io_message(path, &e))
}

fn read_rom_io(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    // Opening a directory succeeds on Unix and only fails on read; check up front
    // so both platforms give the same message.
    if fs::metadata(path)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "is a directory",
        ));
    }
    let mut data = Vec::new();
    File::open(path)?.take(limit).read_to_end(&mut data)?;
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
    // Detection falls back to the Game Boy when a GBA header's checksum is wrong
    // (DECISIONS #46), so a refusal that says "not a Game Boy ROM" names the way out.
    let gba_hint = if archive.is_none()
        && matches!(
            e,
            CartridgeError::TooSmall(1..)
                | CartridgeError::TooLarge(_)
                | CartridgeError::UnsupportedType(_)
                | CartridgeError::InvalidRomSize(_)
                | CartridgeError::InvalidRamSize(_)
        ) {
        " (if this is a GBA game, run it with --system gba)"
    } else {
        ""
    };
    format!(
        "{} is not a usable ROM: {reason}{hint}{gba_hint}",
        path.display()
    )
}

/// Message for a GBA ROM the core refused. `archive` is [`archive_kind`] of the
/// file contents.
pub fn gba_cartridge_message(path: &Path, e: &CartError, archive: Option<&str>) -> String {
    let reason = match e {
        CartError::TooSmall(0) => "the file is empty".to_owned(),
        CartError::TooSmall(n) => format!(
            "the file is only {n} bytes; a GBA ROM is at least {} (0xC0) bytes",
            gba_core::cart::HEADER_SIZE
        ),
        // The reader caps the size, so the real one is unknown.
        CartError::TooLarge(_) => {
            "the file is larger than 32 MiB, the biggest ROM a GBA can map".to_owned()
        }
        // Save-related errors cannot come from loading a ROM; keep the core's text.
        CartError::SaveSize { .. }
        | CartError::NoSave
        | CartError::BadRtcFooter(_)
        | CartError::BadRtcTime => e.to_string(),
    };
    let hint = archive.map_or(String::new(), |kind| {
        format!(" (this looks like a {kind} archive; extract the ROM first)")
    });
    format!("{} is not a usable ROM: {reason}{hint}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const P: &str = "g.gb";

    /// A 0xC0-byte GBA header with the fixed byte and a correct checksum.
    fn gba_head() -> Vec<u8> {
        let mut h = vec![0u8; 0xC0];
        h[0xA0..0xA4].copy_from_slice(b"GAME");
        h[0xB2] = 0x96;
        let sum = h[0xA0..=0xBC].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        h[0xBD] = 0u8.wrapping_sub(sum).wrapping_sub(0x19);
        h
    }

    #[test]
    fn the_header_decides_when_the_name_does_not() {
        let head = gba_head();
        assert!(looks_like_gba(&head));
        // A bad checksum, a missing fixed byte, or a short file is not a GBA.
        let mut bad = head.clone();
        bad[0xBD] ^= 1;
        assert!(!looks_like_gba(&bad));
        let mut no96 = head.clone();
        no96[0xB2] = 0x97;
        assert!(!looks_like_gba(&no96));
        assert!(!looks_like_gba(&head[..0xBD]));
        assert!(!looks_like_gba(&[]));
        // A changed title byte breaks the checksum too.
        let mut title = head;
        title[0xA0] ^= 0x10;
        assert!(!looks_like_gba(&title));
        // A plain Game Boy header is not.
        assert!(!looks_like_gba(&vec![0u8; 0x8000]));
    }

    #[test]
    fn system_comes_from_the_flag_then_the_extension_then_the_header() {
        let dir = tempfile::tempdir().unwrap();
        let w = |name: &str, data: &[u8]| {
            let p = dir.path().join(name);
            fs::write(&p, data).unwrap();
            p
        };
        let sys = |name: &str, data: &[u8], forced| load_rom(&w(name, data), forced).map(|r| r.0);
        let gb = vec![0u8; 0x8000];
        // Extension, in any case, even with Game Boy contents.
        assert_eq!(sys("a.gba", &gb, None), Ok(System::Gba));
        assert_eq!(sys("b.GBA", &gb, None), Ok(System::Gba));
        // Otherwise the header.
        assert_eq!(sys("c.bin", &gba_head(), None), Ok(System::Gba));
        assert_eq!(sys("d.gb", &gba_head(), None), Ok(System::Gba));
        assert_eq!(sys("e.gb", &gb, None), Ok(System::Gb));
        let mut bad = gba_head();
        bad[0xBD] ^= 0xFF;
        assert_eq!(sys("f.bin", &bad, None), Ok(System::Gb));
        // --system overrides all of it.
        assert_eq!(sys("g.gba", &gb, Some(System::Gb)), Ok(System::Gb));
        assert_eq!(sys("h.gb", &gb, Some(System::Gba)), Ok(System::Gba));
        // A missing file is reported whichever way the system is chosen.
        for forced in [None, Some(System::Gb)] {
            let e = load_rom(&dir.path().join("nope.gb"), forced).unwrap_err();
            assert!(e.contains("file not found"), "{e}");
        }
        // A tiny file is a Game Boy ROM (the core reports it too small), and the
        // contents come back whole.
        assert_eq!(
            load_rom(&w("i.gb", b"abc"), None),
            Ok((System::Gb, b"abc".to_vec()))
        );
    }

    /// The file is read once with the largest cap and then cut to the detected
    /// system's cap: 8 MiB + 1 for a Game Boy ROM, 32 MiB + 1 for a GBA one.
    #[test]
    fn the_detected_systems_cap_applies_to_the_single_read() {
        let dir = tempfile::tempdir().unwrap();
        let gb_cap = MAX_ROM_SIZE;
        let gba_cap = gba_core::MAX_ROM_SIZE;
        assert!(gb_cap < gba_cap);
        let w = |name: &str, head: &[u8], len: usize| -> PathBuf {
            let mut data = vec![0u8; len];
            data[..head.len()].copy_from_slice(head);
            let p = dir.path().join(name);
            fs::write(&p, data).unwrap();
            p
        };
        let head = gba_head();
        let len_of =
            |p: PathBuf, forced: Option<System>| load_rom(&p, forced).map(|(s, r)| (s, r.len()));

        // Undecided by name: a Game Boy file past its cap reaches the core as
        // cap + 1 bytes (so it reports TooLarge), not the 32 MiB it was read at.
        let big_gb = w("big.bin", &[], gb_cap + 4096);
        assert_eq!(len_of(big_gb, None), Ok((System::Gb, gb_cap + 1)));
        // The same size with a GBA header is whole: it is under the GBA cap.
        let gba_mid = w("mid.bin", &head, gb_cap + 4096);
        assert_eq!(len_of(gba_mid, None), Ok((System::Gba, gb_cap + 4096)));
        // A GBA file past 32 MiB stops at the cap + 1.
        let gba_big = w("huge.bin", &head, gba_cap + 4096);
        assert_eq!(len_of(gba_big, None), Ok((System::Gba, gba_cap + 1)));
        // When the name or flag decides, that system's cap applies directly.
        let named = w("n.gb", &[], gb_cap + 4096);
        assert_eq!(
            len_of(named.clone(), Some(System::Gb)),
            Ok((System::Gb, gb_cap + 1))
        );
        assert_eq!(
            len_of(named, Some(System::Gba)),
            Ok((System::Gba, gb_cap + 4096))
        );
        let dot_gba = w("m.gba", &[], gb_cap + 4096);
        assert_eq!(len_of(dot_gba, None), Ok((System::Gba, gb_cap + 4096)));
    }

    #[test]
    fn limits_come_from_the_cores() {
        assert_eq!(max_rom_bytes(System::Gba), gba_core::MAX_ROM_SIZE as u64);
        assert_eq!(max_rom_bytes(System::Gb), MAX_ROM_SIZE as u64);
    }

    #[test]
    fn a_bios_must_be_exactly_16_kib() {
        let dir = tempfile::tempdir().unwrap();
        let w = |name: &str, len: usize| {
            let p = dir.path().join(name);
            fs::write(&p, vec![7u8; len]).unwrap();
            p
        };
        let ok = read_bios(&w("ok.bin", gba_core::BIOS_SIZE)).unwrap();
        assert_eq!(ok.len(), gba_core::BIOS_SIZE);
        let short = read_bios(&w("short.bin", 100)).unwrap_err();
        assert!(
            short.contains("100 bytes") && short.contains("16384"),
            "{short}"
        );
        assert!(
            read_bios(&w("empty.bin", 0))
                .unwrap_err()
                .contains("0 bytes")
        );
        let big = read_bios(&w("big.bin", 1 << 20)).unwrap_err();
        assert!(big.contains("larger than 16384"), "{big}");
        assert!(read_bios(&w("one.bin", gba_core::BIOS_SIZE + 1)).is_err());
        assert!(
            read_bios(&dir.path().join("nope.bin"))
                .unwrap_err()
                .contains("file not found")
        );
    }

    #[test]
    fn gba_core_errors_map_to_messages() {
        let p = Path::new("g.gba");
        let m = |e: CartError| gba_cartridge_message(p, &e, None);
        assert!(m(CartError::TooSmall(0)).contains("empty"));
        let small = m(CartError::TooSmall(10));
        assert!(
            small.contains("10 bytes") && small.contains("192"),
            "{small}"
        );
        let large = m(CartError::TooLarge(gba_core::MAX_ROM_SIZE + 1));
        assert!(large.contains("larger than 32 MiB") && !large.contains("33554433"));
        assert!(m(CartError::NoSave).contains("no battery"));
        assert!(
            gba_cartridge_message(Path::new("g.zip"), &CartError::TooSmall(4), Some("zip"))
                .contains("zip archive")
        );
        // The real core's refusals, end to end.
        let e = gba_core::Gba::new(vec![0; 10], None).err().unwrap();
        assert!(m(e).contains("10 bytes"));
    }

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
    fn a_refused_game_boy_rom_names_system_gba() {
        // A GBA ROM with a wrong header checksum falls back to the Game Boy core.
        for e in [
            CartridgeError::TooLarge(8 * 1024 * 1024 + 1),
            CartridgeError::UnsupportedType(0xE1),
            CartridgeError::TooSmall(200),
        ] {
            assert!(msg(e).contains("--system gba"));
        }
        assert!(!msg(CartridgeError::TooSmall(0)).contains("--system"));
        assert!(!msg(CartridgeError::NoBattery).contains("--system"));
        let zip = cartridge_message(
            Path::new("g.zip"),
            &CartridgeError::UnsupportedType(0x50),
            Some("zip"),
        );
        assert!(!zip.contains("--system"), "an archive is not a GBA ROM");
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

        let missing = read_rom(&dir.path().join("nope.gb"), System::Gb).unwrap_err();
        assert!(missing.contains("file not found") && missing.contains("nope.gb"));

        let is_dir = read_rom(dir.path(), System::Gb).unwrap_err();
        assert!(is_dir.contains("directory"), "{is_dir}");

        let ok = dir.path().join("ok.gb");
        fs::write(&ok, b"abc").unwrap();
        assert_eq!(read_rom(&ok, System::Gb).unwrap(), b"abc");
    }

    #[test]
    fn oversized_files_are_truncated_at_the_limit_plus_one() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.gb");
        fs::write(&big, vec![0u8; MAX_ROM_SIZE + 4096]).unwrap();
        assert_eq!(read_rom(&big, System::Gb).unwrap().len(), MAX_ROM_SIZE + 1);
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
            let m = read_rom(&f, System::Gb).unwrap_err();
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
