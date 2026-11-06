//! `gb-test --replay` on GBA recordings, through the real binary: a recording
//! made with the codec the frontend uses replays to the frames an in-process
//! replay produces, with a real-BIOS recording needing the user's dump and
//! saying so without it. The ROM and the BIOS are synthetic, so nothing here
//! depends on (or commits) a commercial game.

use std::path::Path;
use std::process::{Command, Output};

use emu_common::Button;
use gba_core::replay::{Event, Header, Record, Recording, Replayer, encode_header, encode_record};
use gba_core::{BIOS_SIZE, CartOptions, RtcMode, SaveChoice};

// ---- a tiny ARM assembler ----------------------------------------------------------

const AL: u32 = 0xE000_0000;

fn imm(v: u32) -> u32 {
    (0..16)
        .find_map(|rot| {
            let r = v.rotate_left(rot * 2);
            (r <= 0xFF).then_some(rot << 8 | r)
        })
        .expect("immediate not encodable")
}
fn mov(rd: u32, v: u32) -> u32 {
    AL | 1 << 25 | 13 << 21 | rd << 12 | imm(v)
}
fn add(rd: u32, rn: u32, v: u32) -> u32 {
    AL | 1 << 25 | 4 << 21 | rn << 16 | rd << 12 | imm(v)
}
fn half(load: bool, rd: u32, rn: u32) -> u32 {
    AL | 1 << 24 | 1 << 23 | 1 << 22 | (load as u32) << 20 | rn << 16 | rd << 12 | 0xB0
}

/// A GBA ROM that copies KEYINPUT into palette entry 0 forever: with no layers
/// enabled the backdrop shows it, so the frame follows the buttons.
fn keypad_rom() -> Vec<u8> {
    let loop_at = 0x0800_0000u32 + 4 * 3;
    let words = [
        mov(0, 0x0400_0000),
        add(0, 0, 0x130), // KEYINPUT
        mov(1, 0x0500_0000),
        half(true, 2, 0),  // loop: ldrh r2, [r0]
        half(false, 2, 1), //       strh r2, [r1]
        AL | 0x0A00_0000 | ((loop_at.wrapping_sub(0x0800_0000 + 4 * 5 + 8) >> 2) & 0xFF_FFFF),
    ];
    let mut rom = vec![0u8; 0x400];
    for (i, w) in words.iter().enumerate() {
        rom[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    rom
}

/// A BIOS whose first two instructions jump to the cartridge (`mov r0,
/// #0x08000000; bx r0`), the rest zero: a stand-in for a real dump.
fn jump_bios() -> Vec<u8> {
    let mut b = vec![0u8; BIOS_SIZE];
    b[..4].copy_from_slice(&mov(0, 0x0800_0000).to_le_bytes());
    b[4..8].copy_from_slice(&0xE12F_FF10u32.to_le_bytes());
    b
}

fn press(button: emu_common::Button, pressed: bool, frame: u64) -> Record {
    Record {
        frame,
        event: Event::Button { button, pressed },
    }
}

fn records() -> Vec<Record> {
    vec![
        press(Button::R, true, 2),
        press(Button::Start, true, 3),
        press(Button::R, false, 4),
        Record {
            frame: 5,
            event: Event::Marker,
        },
        press(Button::A, true, 5),
        Record {
            frame: 8,
            event: Event::End,
        },
    ]
}

fn write_recording(path: &Path, header: &Header) {
    let mut bytes = encode_header(header);
    for r in records() {
        bytes.extend_from_slice(&encode_record(r));
    }
    std::fs::write(path, bytes).unwrap();
}

fn replay(args: &[&Path], extra: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gb-test"))
        .arg("--replay")
        .args(args)
        .args(extra)
        .output()
        .expect("gb-test runs")
}

/// The PNG's size and RGB bytes.
fn read_png(path: &Path) -> (u32, u32, Vec<u8>) {
    let dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
    let mut reader = dec.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    assert_eq!(info.color_type, png::ColorType::Rgb);
    assert_eq!(info.bit_depth, png::BitDepth::Eight);
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

fn rgb(frame: &[u16]) -> Vec<u8> {
    frame.iter().flat_map(|&c| gba_core_rgb(c)).collect()
}

/// RGB555 widened with no correction (the rule the frontend's screenshots use).
fn gba_core_rgb(c: u16) -> [u8; 3] {
    let wide = |v: u16| ((v << 3) | (v >> 2)) as u8;
    [wide(c & 31), wide(c >> 5 & 31), wide(c >> 10 & 31)]
}

fn header_for(rom: &[u8], bios_hash: Option<u64>) -> Header {
    Header {
        rom_hash: gba_core::fnv1a(rom),
        battery: None,
        options: CartOptions {
            save: SaveChoice::Auto,
            rtc: RtcMode::Auto,
        },
        bios_hash,
        rtc_start: None,
    }
}

#[test]
fn a_gba_recording_replays_to_the_frames_an_in_process_replay_makes() {
    let dir = tempfile::tempdir().unwrap();
    let rom = keypad_rom();
    let (rec_path, rom_path, out) = (
        dir.path().join("s.gbinput"),
        dir.path().join("g.gba"),
        dir.path().join("out"),
    );
    std::fs::write(&rom_path, &rom).unwrap();
    write_recording(&rec_path, &header_for(&rom, None));

    let o = replay(&[&rec_path, &rom_path], &["--out", out.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(
        stdout.contains("GBA") && stdout.contains("markers [5]"),
        "{stdout}"
    );

    // Marker frame 5 and the last frame, 8; compare with the library replay.
    let rec = Recording::decode(&std::fs::read(&rec_path).unwrap()).unwrap();
    let mut gba = rec.power_on(rom, None).unwrap();
    let mut player = Replayer::new(&rec);
    let mut previous = None;
    for frame in [5u64, 8] {
        while player.frame() < frame {
            player.step_frame(&mut gba);
        }
        let (w, h, pixels) = read_png(&out.join(format!("f{frame:07}.png")));
        assert_eq!((w, h), (240, 160));
        assert_eq!(pixels, rgb(&gba.framebuffer()[..]), "frame {frame}");
        assert_ne!(
            Some(&pixels),
            previous.as_ref(),
            "the buttons changed the picture"
        );
        previous = Some(pixels);
    }
    // Start (bit 3) and A (bit 0) are held at frame 8, R was let go: KEYINPUT 0x3F6.
    assert_eq!(
        gba.framebuffer()[0] & 0x3FF,
        0x3F6,
        "palette 0 follows KEYINPUT"
    );
    assert!(!dir.path().join("out/f0000006.png").exists());
}

#[test]
fn an_explicit_dump_list_replaces_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let rom = keypad_rom();
    let (rec_path, rom_path, out) = (
        dir.path().join("s.gbinput"),
        dir.path().join("g.gba"),
        dir.path().join("out"),
    );
    std::fs::write(&rom_path, &rom).unwrap();
    write_recording(&rec_path, &header_for(&rom, None));
    let o = replay(
        &[&rec_path, &rom_path],
        &["--dump", "1,3", "--out", out.to_str().unwrap()],
    );
    assert!(o.status.success());
    assert!(out.join("f0000001.png").is_file() && out.join("f0000003.png").is_file());
    assert!(!out.join("f0000005.png").exists());
}

#[test]
fn a_real_bios_recording_needs_the_users_dump() {
    let dir = tempfile::tempdir().unwrap();
    let rom = keypad_rom();
    let bios = jump_bios();
    let (rec_path, rom_path, bios_path, other_path) = (
        dir.path().join("s.gbinput"),
        dir.path().join("g.gba"),
        dir.path().join("bios.bin"),
        dir.path().join("other.bin"),
    );
    std::fs::write(&rom_path, &rom).unwrap();
    std::fs::write(&bios_path, &bios).unwrap();
    let mut other = bios.clone();
    other[100] = 1;
    std::fs::write(&other_path, &other).unwrap();
    write_recording(&rec_path, &header_for(&rom, Some(gba_core::fnv1a(&bios))));
    let run = |extra: &[&str], out: &str| {
        let out = dir.path().join(out);
        let mut args = vec!["--out", out.to_str().unwrap()];
        args.extend_from_slice(extra);
        let o = replay(&[&rec_path, &rom_path], &args);
        (
            o.status.success(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
            out,
        )
    };

    // No dump: a clear error naming --bios.
    let (ok, err, _) = run(&[], "a");
    assert!(
        !ok && err.contains("real BIOS") && err.contains("--bios"),
        "{err}"
    );
    // Another dump, a wrong-sized file.
    let (ok, err, _) = run(&["--bios", other_path.to_str().unwrap()], "b");
    assert!(!ok && err.contains("different BIOS"), "{err}");
    let (ok, err, _) = run(&["--bios", rom_path.to_str().unwrap()], "c");
    assert!(!ok && err.contains("a GBA BIOS is 16384"), "{err}");
    // The right dump boots through the BIOS and replays the same frames as the
    // library does with it.
    let (ok, err, out) = run(&["--bios", bios_path.to_str().unwrap()], "d");
    assert!(ok, "{err}");
    let rec = Recording::decode(&std::fs::read(&rec_path).unwrap()).unwrap();
    let mut gba = rec
        .power_on(rom.clone(), Some(bios.clone().into_boxed_slice()))
        .unwrap();
    let mut player = Replayer::new(&rec);
    while player.frame() < 8 {
        player.step_frame(&mut gba);
    }
    assert_eq!(
        read_png(&out.join("f0000008.png")).2,
        rgb(&gba.framebuffer()[..])
    );
    // A built-in-BIOS recording refuses a dump instead of replaying differently.
    write_recording(&rec_path, &header_for(&rom, None));
    let (ok, err, _) = run(&["--bios", bios_path.to_str().unwrap()], "e");
    assert!(!ok && err.contains("built-in BIOS"), "{err}");
}

#[test]
fn damaged_and_foreign_recordings_are_named() {
    let dir = tempfile::tempdir().unwrap();
    let rom = keypad_rom();
    let (rec_path, rom_path) = (dir.path().join("s.gbinput"), dir.path().join("g.gba"));
    std::fs::write(&rom_path, &rom).unwrap();
    // Another ROM.
    let mut other = rom.clone();
    other[0x200] = 9;
    write_recording(&rec_path, &header_for(&other, None));
    let o = replay(&[&rec_path, &rom_path], &[]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("different ROM"));
    // Garbage, and a bad record in an otherwise good GBA file.
    std::fs::write(&rec_path, b"not a recording at all").unwrap();
    let o = replay(&[&rec_path, &rom_path], &[]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("not an input recording"));
    let mut bytes = encode_header(&header_for(&rom, None));
    bytes.extend_from_slice(&encode_record(press(Button::A, true, 1)));
    let last = bytes.len() - 1;
    bytes[last] = 0x3F; // not a record kind
    std::fs::write(&rec_path, &bytes).unwrap();
    let o = replay(&[&rec_path, &rom_path], &[]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("unknown kind"));
    // --bios on a Game Boy recording is a mistake.
    let gb = gb_core::replay::encode_header(&gb_core::replay::Header {
        rom_hash: 1,
        battery: None,
        model: gb_core::Model::Dmg,
    });
    std::fs::write(&rec_path, gb).unwrap();
    let o = replay(&[&rec_path, &rom_path], &["--bios", "x"]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("--bios is for GBA"));
}
