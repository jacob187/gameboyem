//! Game Boy Advance cases for the harness: the `-gba` kinds in `expected.txt`, plus the
//! `--dump-gba` screenshot aid. DECISIONS #57 has the rationale.
//!
//! * `jsmolka-gba`: jsmolka/gba-tests end in a single `b .` (`0xEAFFFFFE`). The runner finds that
//!   word in the ROM, steps instruction by instruction until PC sits on it, and reads the
//!   failed-test register (r12, r7 for `thumb.gba`): 0 is a pass, N is the first failed test.
//! * `fuzzarm-gba`: FuzzARM leaves EWRAM 0x0200_0000 at 0 until a test fails, then dumps the
//!   failure there and waits for a key. A pass also needs the ROM to reach its own idle `b .`,
//!   otherwise a CPU that never ran would look like a pass.
//! * `smoke-gba`: the framebuffer is not one flat colour after the frame budget.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use gb_core::rgb555_to_rgb888;
use gba_core::{CYCLES_PER_FRAME, Gba, SCREEN_HEIGHT, SCREEN_WIDTH};

use crate::{Case, Kind, Outcome, artifact_dir, save_sized_png};

/// ARM `b .`: the idle loop every jsmolka ROM (and FuzzARM's `mainloop`) ends in.
const IDLE_WORD: u32 = 0xEAFF_FFFE;
/// Where the cartridge is mapped (wait-state 0 mirror).
const ROM_BASE: u32 = 0x0800_0000;
const EWRAM_BASE: u32 = 0x0200_0000;
/// Size of FuzzARM's failure record (16 words).
const FUZZ_DUMP_LEN: usize = 64;
/// Frames FuzzARM is given, after the first nonzero EWRAM word, to finish writing its record.
const FUZZ_SETTLE_FRAMES: u32 = 10;

/// Dispatch a GBA case. `rom` is the file's bytes.
pub fn run_case(case: &Case, rom: Vec<u8>) -> Outcome {
    let file = case.path.rsplit('/').next().unwrap_or(&case.path);
    let idle = match case.kind {
        Kind::JsmolkaGba | Kind::FuzzarmGba => match find_idle(&rom) {
            Ok(a) => a,
            Err(e) => return Outcome::Fail(e),
        },
        _ => 0,
    };
    let mut gba = match Gba::new(rom, None) {
        Ok(g) => g,
        Err(e) => return Outcome::Fail(format!("load error: {e}")),
    };
    match case.kind {
        Kind::JsmolkaGba => run_jsmolka(&mut gba, idle, fail_register(file), case.max_frames),
        Kind::FuzzarmGba => run_fuzzarm(&mut gba, idle, case.max_frames),
        _ => run_smoke(&mut gba, case),
    }
}

/// The address of the ROM's single aligned `b .` word.
pub fn find_idle(rom: &[u8]) -> Result<u32, String> {
    let mut hits = rom
        .chunks_exact(4)
        .enumerate()
        .filter(|(_, w)| u32::from_le_bytes([w[0], w[1], w[2], w[3]]) == IDLE_WORD)
        .map(|(i, _)| ROM_BASE + 4 * i as u32);
    match (hits.next(), hits.next()) {
        (Some(a), None) => Ok(a),
        (None, _) => Err("no `b .` (0xEAFFFFFE) word in the ROM to use as the idle address".into()),
        (Some(a), Some(b)) => Err(format!(
            "more than one `b .` word in the ROM (first at {a:08X}, {b:08X}): idle address is ambiguous"
        )),
    }
}

/// The register jsmolka's `m_test_eval` leaves the failed-test number in: r7 for the Thumb
/// ROM, r12 for the rest. Keyed on the file name, as there is exactly one Thumb ROM.
pub fn fail_register(file_name: &str) -> usize {
    if file_name == "thumb.gba" { 7 } else { 12 }
}

/// Turn the failed-test register into an outcome.
pub fn jsmolka_result(reg: usize, value: u32) -> Outcome {
    match value {
        0 => Outcome::Pass,
        n => Outcome::Fail(format!("failed test {n} (r{reg})")),
    }
}

fn run_jsmolka(gba: &mut Gba, idle: u32, reg: usize, max_frames: u32) -> Outcome {
    let budget = u64::from(max_frames) * u64::from(CYCLES_PER_FRAME);
    loop {
        let r = gba.cpu_registers();
        if r.r[15] == idle {
            return jsmolka_result(reg, r.r[reg]);
        }
        if gba.cycles() >= budget {
            return Outcome::Timeout(format!(
                "idle loop {idle:08X} not reached in {max_frames} frames; PC={:08X}",
                r.r[15]
            ));
        }
        gba.step_instruction();
    }
}

fn ewram_word(gba: &Gba, offset: u32) -> u32 {
    (0..4).fold(0, |w, i| {
        w | u32::from(gba.peek8(EWRAM_BASE + offset + i)) << (8 * i)
    })
}

fn run_fuzzarm(gba: &mut Gba, idle: u32, max_frames: u32) -> Outcome {
    for _ in 0..max_frames {
        gba.step_frame();
        if ewram_word(gba, 0) != 0 {
            for _ in 0..FUZZ_SETTLE_FRAMES {
                gba.step_frame();
            }
            let dump: Vec<u8> = (0..FUZZ_DUMP_LEN as u32)
                .map(|i| gba.peek8(EWRAM_BASE + i))
                .collect();
            return Outcome::Fail(decode_fuzzarm(&dump));
        }
        if gba.cpu_registers().r[15] == idle {
            return Outcome::Pass;
        }
    }
    Outcome::Timeout(format!(
        "idle loop {idle:08X} not reached in {max_frames} frames; PC={:08X}",
        gba.cpu_registers().r[15]
    ))
}

/// Decode FuzzARM's EWRAM failure record (64 bytes from 0x0200_0000): `AAAA`/`TTTT`, the opcode
/// name (12 characters), initial r0-r2 and CPSR, got r3, r4, 0, CPSR, expected r3, r4, 0, CPSR.
/// Only the registers that differ are listed, the inputs come last.
pub fn decode_fuzzarm(dump: &[u8]) -> String {
    if dump.len() < FUZZ_DUMP_LEN {
        return format!("short FuzzARM dump ({} bytes)", dump.len());
    }
    let word = |i: usize| u32::from_le_bytes(dump[4 * i..4 * i + 4].try_into().unwrap());
    let state = match &dump[..4] {
        b"AAAA" => "ARM",
        b"TTTT" => "THUMB",
        _ => return format!("unrecognised FuzzARM dump header {:08X}", word(0)),
    };
    let name = String::from_utf8_lossy(&dump[4..16])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut diffs = Vec::new();
    for (label, got, want) in [
        ("r3", word(8), word(12)),
        ("r4", word(9), word(13)),
        ("cpsr", word(11), word(15)),
    ] {
        if got != want {
            diffs.push(format!("{label} got {got:08X} want {want:08X}"));
        }
    }
    format!(
        "{state} {name}: {} (in r0={:08X} r1={:08X} r2={:08X} cpsr={:08X})",
        diffs.join(", "),
        word(4),
        word(5),
        word(6),
        word(7)
    )
}

fn run_smoke(gba: &mut Gba, case: &Case) -> Outcome {
    for _ in 0..case.max_frames {
        gba.step_frame();
    }
    let fb = gba.framebuffer();
    if !fb.iter().all(|&p| p == fb[0]) {
        return Outcome::Pass;
    }
    let out = artifact_dir().join(format!("{}.png", case.name().replace('/', "_")));
    let saved = save_gba_png(fb, &out)
        .map(|()| format!("; frame written to {}", out.display()))
        .unwrap_or_default();
    Outcome::Fail(format!(
        "framebuffer uniform ({:04X}) after {} frames{saved}",
        fb[0], case.max_frames
    ))
}

/// Write a GBA frame (`SCREEN_WIDTH` x `SCREEN_HEIGHT` RGB555) as an RGB PNG.
pub fn save_gba_png(fb: &[u16], path: &Path) -> Result<(), String> {
    if fb.len() != SCREEN_WIDTH * SCREEN_HEIGHT {
        return Err(format!("framebuffer has {} pixels", fb.len()));
    }
    let rgb: Vec<u8> = fb.iter().flat_map(|&c| rgb555_to_rgb888(c)).collect();
    save_sized_png(
        SCREEN_WIDTH as u32,
        SCREEN_HEIGHT as u32,
        &rgb,
        png::ColorType::Rgb,
        path,
    )
}

const DUMP_USAGE: &str =
    "usage: gb-test --dump-gba ROM FRAMES OUT_DIR   (FRAMES: comma-separated frame numbers)";
/// Highest frame `--dump-gba` accepts (about 10 minutes of emulated time).
const DUMP_MAX_FRAME: u64 = 36_000;

/// `gb-test --dump-gba ROM FRAMES OUT_DIR`: run a ROM headlessly and write
/// `OUT_DIR/<rom>-<frame>.png` after each listed frame (0 is the power-on screen).
pub fn dump_main(args: &[String]) -> ExitCode {
    match dump(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}\n{DUMP_USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// Parse `60,120,300` into a sorted, de-duplicated frame list.
pub fn parse_frames(text: &str) -> Result<Vec<u64>, String> {
    let mut frames = text
        .split(',')
        .map(|f| {
            let n: u64 = f
                .trim()
                .parse()
                .map_err(|_| format!("bad frame number {f:?}"))?;
            if n > DUMP_MAX_FRAME {
                return Err(format!("frame {n} is past the {DUMP_MAX_FRAME} limit"));
            }
            Ok(n)
        })
        .collect::<Result<Vec<_>, _>>()?;
    frames.sort_unstable();
    frames.dedup();
    Ok(frames)
}

fn dump(args: &[String]) -> Result<(), String> {
    let positional: Vec<&String> = args.iter().filter(|a| *a != "--dump-gba").collect();
    if let Some(o) = positional.iter().find(|a| a.starts_with("--")) {
        return Err(format!("unknown option `{o}`"));
    }
    let [rom_path, frames, out_dir] = positional[..] else {
        return Err("expected a ROM, a frame list and an output directory".into());
    };
    let frames = parse_frames(frames)?;
    let rom = std::fs::read(rom_path).map_err(|e| format!("{rom_path}: {e}"))?;
    let mut gba = Gba::new(rom, None).map_err(|e| format!("load error: {e}"))?;
    let stem = PathBuf::from(rom_path)
        .file_stem()
        .map_or_else(|| "rom".into(), |s| s.to_string_lossy().into_owned());
    for f in frames {
        while gba.frames() < f {
            gba.step_frame();
        }
        let out = Path::new(out_dir).join(format!("{stem}-{f}.png"));
        save_gba_png(gba.framebuffer(), &out)?;
        println!("{}", out.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_manifest;

    /// A ROM image of `words` (little-endian), padded to a plausible size.
    fn rom_of(words: &[u32]) -> Vec<u8> {
        let mut rom: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        rom.resize(0x200, 0);
        rom
    }

    #[test]
    fn idle_address_is_the_single_branch_to_self() {
        let rom = rom_of(&[0xE1A0_0000, 0xE1A0_0000, IDLE_WORD, 0]);
        assert_eq!(find_idle(&rom), Ok(0x0800_0008));
    }

    #[test]
    fn idle_address_must_exist_and_be_unique() {
        assert!(
            find_idle(&rom_of(&[0xE1A0_0000]))
                .unwrap_err()
                .contains("no `b .`")
        );
        let err = find_idle(&rom_of(&[IDLE_WORD, 0, IDLE_WORD])).unwrap_err();
        assert!(
            err.contains("08000000") && err.contains("08000008"),
            "{err}"
        );
        // An unaligned occurrence is not an instruction.
        let mut rom = vec![0; 8];
        rom[1..5].copy_from_slice(&IDLE_WORD.to_le_bytes());
        assert!(find_idle(&rom).is_err());
    }

    #[test]
    fn thumb_rom_reports_in_r7_and_the_rest_in_r12() {
        assert_eq!(fail_register("thumb.gba"), 7);
        for f in [
            "arm.gba",
            "memory.gba",
            "nes.gba",
            "sram.gba",
            "flash64.gba",
        ] {
            assert_eq!(fail_register(f), 12, "{f}");
        }
    }

    #[test]
    fn jsmolka_register_decodes_to_pass_or_a_test_number() {
        assert_eq!(jsmolka_result(12, 0), Outcome::Pass);
        assert_eq!(
            jsmolka_result(12, 204),
            Outcome::Fail("failed test 204 (r12)".into())
        );
        assert_eq!(jsmolka_result(7, 3).detail(), "failed test 3 (r7)");
    }

    #[test]
    fn jsmolka_runner_passes_a_rom_that_idles_with_a_clear_register() {
        // Entry point is the idle loop and r12 starts at 0.
        let rom = rom_of(&[IDLE_WORD]);
        let idle = find_idle(&rom).unwrap();
        let mut gba = Gba::new(rom, None).unwrap();
        assert_eq!(run_jsmolka(&mut gba, idle, 12, 2), Outcome::Pass);
    }

    /// A FuzzARM failure record, with the given state tag and name.
    fn fuzz_dump(tag: &[u8; 4], name: &str, words: [u32; 12]) -> Vec<u8> {
        let mut d = tag.to_vec();
        let mut n = name.as_bytes().to_vec();
        n.resize(12, b' ');
        d.extend(n);
        d.extend(words.iter().flat_map(|w| w.to_le_bytes()));
        d
    }

    #[test]
    fn fuzzarm_dump_lists_only_the_registers_that_differ() {
        let d = fuzz_dump(
            b"AAAA",
            "add lsl ",
            [
                1,
                2,
                3,
                0x6000_0000, // initial r0 r1 r2 cpsr
                0x10,
                0x20,
                0,
                0x2000_0000, // got r3 r4 - cpsr
                0x10,
                0x21,
                0,
                0x4000_0000, // expected
            ],
        );
        assert_eq!(
            decode_fuzzarm(&d),
            "ARM add lsl: r4 got 00000020 want 00000021, cpsr got 20000000 want 40000000 \
             (in r0=00000001 r1=00000002 r2=00000003 cpsr=60000000)"
        );
    }

    #[test]
    fn fuzzarm_dump_names_thumb_and_load_store_opcodes() {
        let d = fuzz_dump(b"TTTT", "strh/ldrsh", [0, 0, 0, 0, 5, 0, 0, 0, 6, 0, 0, 0]);
        let s = decode_fuzzarm(&d);
        assert!(
            s.starts_with("THUMB strh/ldrsh: r3 got 00000005 want 00000006"),
            "{s}"
        );
    }

    #[test]
    fn fuzzarm_dump_rejects_garbage() {
        assert!(decode_fuzzarm(&[0; 10]).starts_with("short FuzzARM dump"));
        let mut d = fuzz_dump(b"AAAA", "x", [0; 12]);
        d[0] = b'Q';
        assert!(decode_fuzzarm(&d).starts_with("unrecognised FuzzARM dump header"));
    }

    #[test]
    fn gba_suffix_selects_the_gba_kinds() {
        let cases = parse_manifest(
            "fail jsmolka-gba 60 test/gba/jsmolka/arm/arm.gba\n\
             fail fuzzarm-gba 600 test/gba/fuzzarm/ARM_Any.gba\n\
             fail smoke-gba 120 test/gba/tonc/bin/m3_demo.gba\n\
             pass smoke 10 a.gb\n",
        );
        let kinds: Vec<_> = cases.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                Kind::JsmolkaGba,
                Kind::FuzzarmGba,
                Kind::SmokeGba,
                Kind::Smoke
            ]
        );
        assert_eq!(cases[0].name(), "gba/jsmolka/arm/arm");
        assert_eq!(cases[0].max_frames, 60);
    }

    #[test]
    #[should_panic(expected = "no `-gba` variant")]
    fn gba_suffix_on_a_gb_only_kind_is_rejected() {
        parse_manifest("pass mooneye-gba 10 a.gba\n");
    }

    #[test]
    #[should_panic(expected = "bad kind")]
    fn jsmolka_needs_the_gba_suffix() {
        parse_manifest("pass jsmolka 10 a.gba\n");
    }

    #[test]
    fn frame_lists_parse_sorted_and_bounded() {
        assert_eq!(parse_frames("120, 60,60,0"), Ok(vec![0, 60, 120]));
        assert!(parse_frames("1,,2").is_err());
        assert!(parse_frames("x").is_err());
        assert!(parse_frames("36001").is_err());
    }

    #[test]
    fn gba_png_is_240x160_rgb_with_the_readme_widening() {
        let mut fb = vec![0u16; SCREEN_WIDTH * SCREEN_HEIGHT];
        fb[0] = 0x001F; // pure red
        fb[SCREEN_WIDTH * SCREEN_HEIGHT - 1] = 0b01111_00001_10000;
        let dir = std::env::temp_dir().join(format!("gb-test-gba-png-{}", std::process::id()));
        let path = dir.join("f.png");
        save_gba_png(&fb, &path).unwrap();
        let mut reader =
            png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()))
                .read_info()
                .unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (240, 160));
        assert_eq!(info.color_type, png::ColorType::Rgb);
        assert_eq!(buf[..3], [255, 0, 0]);
        assert_eq!(buf[buf.len() - 3..], [0x84, 0x08, 0x7B]);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(save_gba_png(&fb[1..], &path).is_err());
    }
}
