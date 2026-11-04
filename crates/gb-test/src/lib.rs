//! Headless test-ROM harness for `gb-core`.
//!
//! Never opens a window: runs on macOS, in Docker and in CI. The list of ROMs and
//! their expected outcomes lives in `expected.txt` (see its header).
//!
//! ROMs are looked up under `$GB_ROMS_DIR` if set, else `<workspace>/roms`.

use std::fmt;
use std::path::{Path, PathBuf};

use gb_core::{Button, GameBoy, SCREEN_HEIGHT, SCREEN_WIDTH};

/// The manifest, compiled in so tests and the CLI always agree.
pub const MANIFEST: &str = include_str!("../expected.txt");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Blargg: result printed to the serial port.
    Blargg,
    /// Blargg (newer ROMs): result in cartridge RAM -- signature `DE B0 61` at
    /// A001-A003, status at A000 (0x80 = running, 0 = pass), text from A004.
    BlarggMem,
    /// Mooneye: `LD B,B` with Fibonacci registers on pass.
    Mooneye,
    /// dmg-acid2: framebuffer compared to the reference image.
    Acid2,
    /// Homebrew smoke test: framebuffer not blank after N frames.
    Smoke,
    /// Homebrew playability: boot snake, press Start, steer, framebuffer must change.
    PlaySnake,
    /// Homebrew playability: boot breakout, press Start, launch, move the paddle.
    PlayBreakout,
}

#[derive(Clone, Debug)]
pub struct Case {
    /// Path relative to the ROMs directory.
    pub path: String,
    pub kind: Kind,
    pub expect_pass: bool,
    pub max_frames: u32,
}

impl Case {
    /// Short display name, e.g. `blargg/cpu_instrs/individual/01-special`.
    pub fn name(&self) -> String {
        let p = self.path.strip_prefix("test/").unwrap_or(&self.path);
        let p = p.strip_suffix(".gb").unwrap_or(p);
        match self.kind {
            // Same ROM as the smoke case; keep the names unique.
            Kind::PlaySnake | Kind::PlayBreakout => format!("{p}-play"),
            _ => p.to_string(),
        }
    }

    pub fn rom_path(&self) -> PathBuf {
        roms_dir().join(&self.path)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail(String),
    Timeout(String),
    Missing,
}

impl Outcome {
    pub fn passed(&self) -> bool {
        matches!(self, Outcome::Pass)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Pass => "PASS",
            Outcome::Fail(_) => "FAIL",
            Outcome::Timeout(_) => "TIMEOUT",
            Outcome::Missing => "MISSING",
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Outcome::Fail(d) | Outcome::Timeout(d) => d,
            _ => "",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.detail() {
            "" => f.write_str(self.label()),
            d => write!(f, "{}: {d}", self.label()),
        }
    }
}

/// Root directory holding `test/` and `homebrew/` ROMs.
pub fn roms_dir() -> PathBuf {
    match std::env::var_os("GB_ROMS_DIR") {
        Some(d) => PathBuf::from(d),
        None => workspace_root().join("roms"),
    }
}

/// Repository root (two levels above this crate).
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// True when running in CI, where missing ROMs are an error, not a skip.
pub fn in_ci() -> bool {
    std::env::var_os("CI").is_some_and(|v| !v.is_empty() && v != "false")
}

/// Parse `expected.txt`. Panics on malformed lines (it is a compiled-in file).
pub fn load_manifest() -> Vec<Case> {
    parse_manifest(MANIFEST)
}

pub fn parse_manifest(text: &str) -> Vec<Case> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|(n, line)| {
            let mut parts = line.trim().splitn(4, ' ');
            let mut next = |what| {
                parts
                    .next()
                    .unwrap_or_else(|| panic!("expected.txt:{}: missing {what}", n + 1))
            };
            let expect_pass = match next("expectation") {
                "pass" => true,
                "fail" => false,
                o => panic!("expected.txt:{}: bad expectation {o:?}", n + 1),
            };
            let kind = match next("kind") {
                "blargg" => Kind::Blargg,
                "blargg-mem" => Kind::BlarggMem,
                "mooneye" => Kind::Mooneye,
                "acid2" => Kind::Acid2,
                "smoke" => Kind::Smoke,
                "play-snake" => Kind::PlaySnake,
                "play-breakout" => Kind::PlayBreakout,
                o => panic!("expected.txt:{}: bad kind {o:?}", n + 1),
            };
            let max_frames = next("max_frames")
                .parse()
                .unwrap_or_else(|e| panic!("expected.txt:{}: bad frame count: {e}", n + 1));
            let path = next("path").to_string();
            Case {
                path,
                kind,
                expect_pass,
                max_frames,
            }
        })
        .collect()
}

/// Run one case to completion (or its frame limit).
pub fn run_case(case: &Case) -> Outcome {
    let path = case.rom_path();
    let Ok(rom) = std::fs::read(&path) else {
        return Outcome::Missing;
    };
    match case.kind {
        Kind::PlaySnake => return run_play(&rom, PLAY_SNAKE, case.max_frames),
        Kind::PlayBreakout => return run_play(&rom, PLAY_BREAKOUT, case.max_frames),
        _ => {}
    }
    let mut gb = match GameBoy::new(rom) {
        Ok(gb) => gb,
        Err(e) => return Outcome::Fail(format!("load error: {e}")),
    };
    match case.kind {
        Kind::Blargg => run_blargg(&mut gb, case.max_frames),
        Kind::BlarggMem => run_blargg_mem(&mut gb, case.max_frames),
        Kind::Mooneye => run_mooneye(&mut gb, case.max_frames),
        Kind::Acid2 => run_acid2(&mut gb, case),
        Kind::Smoke => run_smoke(&mut gb, case.max_frames),
        Kind::PlaySnake | Kind::PlayBreakout => unreachable!("handled above"),
    }
}

fn locked_message(gb: &GameBoy) -> String {
    let r = gb.cpu_registers();
    format!(
        "CPU locked (illegal/unimplemented opcode) near PC={:04X}",
        r.pc.wrapping_sub(1)
    )
}

fn serial_tail(gb: &GameBoy) -> String {
    let s = String::from_utf8_lossy(gb.serial_output());
    let s = s.trim();
    let tail: String = s
        .chars()
        .rev()
        .take(200)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    tail.replace('\n', " | ")
}

fn run_blargg(gb: &mut GameBoy, max_frames: u32) -> Outcome {
    for _ in 0..max_frames {
        gb.step_frame();
        let out = gb.serial_output();
        if contains(out, b"Failed") {
            // Let it finish printing which tests failed.
            for _ in 0..30 {
                gb.step_frame();
            }
            return Outcome::Fail(serial_tail(gb));
        }
        if contains(out, b"Passed") {
            return Outcome::Pass;
        }
        if gb.cpu_locked() {
            return Outcome::Fail(format!(
                "{}; serial: {}",
                locked_message(gb),
                serial_tail(gb)
            ));
        }
    }
    Outcome::Timeout(format!("serial: {:?}", serial_tail(gb)))
}

/// Blargg's memory protocol: `DE B0 61` at A001-A003 marks the block valid,
/// A000 holds 0x80 while running and then the result code (0 = pass), and a
/// NUL-terminated report starts at A004.
fn run_blargg_mem(gb: &mut GameBoy, max_frames: u32) -> Outcome {
    const STATUS: u16 = 0xA000;
    const TEXT: u16 = 0xA004;
    let report = |gb: &GameBoy| {
        let text: Vec<u8> = (0..0x1000u16)
            .map(|i| gb.peek(TEXT + i))
            .take_while(|&b| b != 0)
            .collect();
        let text = String::from_utf8_lossy(&text);
        let mut lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        // The ROM ends with "Passed"/"Failed" lines; keep the tail short.
        lines.drain(..lines.len().saturating_sub(6));
        lines.join(" | ")
    };
    for _ in 0..max_frames {
        gb.step_frame();
        if gb.cpu_locked() {
            return Outcome::Fail(format!("{}; report: {}", locked_message(gb), report(gb)));
        }
        let signed = [0xDE, 0xB0, 0x61]
            .iter()
            .enumerate()
            .all(|(i, &b)| gb.peek(STATUS + 1 + i as u16) == b);
        if !signed {
            continue;
        }
        match gb.peek(STATUS) {
            0x80 => {}
            0 => return Outcome::Pass,
            code => return Outcome::Fail(format!("code {code:#04X}: {}", report(gb))),
        }
    }
    Outcome::Timeout(format!("status {:#04X}: {}", gb.peek(STATUS), report(gb)))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn run_mooneye(gb: &mut GameBoy, max_frames: u32) -> Outcome {
    let budget = max_frames as u64 * gb_core::M_CYCLES_PER_FRAME as u64;
    let start = gb.mcycles();
    while gb.mcycles() - start < budget {
        gb.step_instruction();
        if gb.take_ld_b_b_breakpoint() {
            let r = gb.cpu_registers();
            let regs = [r.b, r.c, r.d, r.e, r.h, r.l];
            return if regs == [3, 5, 8, 13, 21, 34] {
                Outcome::Pass
            } else if regs == [0x42; 6] {
                Outcome::Fail("test reported failure (registers = 0x42)".into())
            } else {
                Outcome::Fail(format!(
                    "LD B,B with unexpected registers B={:02X} C={:02X} D={:02X} E={:02X} H={:02X} L={:02X}",
                    r.b, r.c, r.d, r.e, r.h, r.l
                ))
            };
        }
        if gb.cpu_locked() {
            return Outcome::Fail(locked_message(gb));
        }
    }
    Outcome::Timeout(format!("no LD B,B within {max_frames} frames"))
}

/// Load a reference PNG and map greyscale values to DMG shades
/// (0xFF→0 lightest, 0xAA→1, 0x55→2, 0x00→3), per the dmg-acid2 README.
pub fn load_reference(path: &Path) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("png too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    if info.width as usize != SCREEN_WIDTH || info.height as usize != SCREEN_HEIGHT {
        return Err(format!("reference is {}x{}", info.width, info.height));
    }
    let channels = info.color_type.samples();
    buf[..info.buffer_size()]
        .chunks_exact(channels)
        .map(|px| match px[0] {
            0xFF => Ok(0),
            0xAA => Ok(1),
            0x55 => Ok(2),
            0x00 => Ok(3),
            v => Err(format!("unexpected grey value {v:#04X} in reference")),
        })
        .collect()
}

/// Write a framebuffer as a greyscale PNG (for debugging failed comparisons).
pub fn save_framebuffer_png(fb: &[u8], path: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
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

/// Where failing-test artifacts (actual framebuffers) are written.
pub fn artifact_dir() -> PathBuf {
    workspace_root().join("target").join("gb-test-out")
}

fn run_acid2(gb: &mut GameBoy, case: &Case) -> Outcome {
    let reference_path = case.rom_path().with_file_name("reference-dmg.png");
    let reference = match load_reference(&reference_path) {
        Ok(r) => r,
        Err(e) => return Outcome::Fail(format!("reference image: {e}")),
    };
    // The test finishes setup within a few frames and signals with LD B,B;
    // render a couple more frames after it (or stop at the frame limit).
    let mut frames_after_signal = None;
    for _ in 0..case.max_frames {
        gb.step_frame();
        if gb.take_ld_b_b_breakpoint() && frames_after_signal.is_none() {
            frames_after_signal = Some(0);
        }
        if let Some(n) = frames_after_signal.as_mut() {
            *n += 1;
            if *n >= 3 {
                break;
            }
        }
        if gb.cpu_locked() {
            return Outcome::Fail(locked_message(gb));
        }
    }
    let fb = gb.framebuffer();
    let diff = fb.iter().zip(&reference).filter(|(a, b)| a != b).count();
    if diff == 0 {
        Outcome::Pass
    } else {
        let out = artifact_dir().join(format!("{}.png", case.name().replace('/', "_")));
        let saved = save_framebuffer_png(fb, &out)
            .map(|()| format!("; actual frame written to {}", out.display()))
            .unwrap_or_default();
        Outcome::Fail(format!("{diff} pixels differ from reference{saved}"))
    }
}

fn run_smoke(gb: &mut GameBoy, frames: u32) -> Outcome {
    for _ in 0..frames {
        gb.step_frame();
        if gb.cpu_locked() {
            return Outcome::Fail(locked_message(gb));
        }
    }
    let fb = gb.framebuffer();
    if fb.iter().all(|&p| p == fb[0]) {
        Outcome::Fail(format!(
            "framebuffer blank (all shade {}) after {frames} frames",
            fb[0]
        ))
    } else {
        Outcome::Pass
    }
}

/// Frames the title screen gets before the script presses Start.
const PLAY_BOOT_FRAMES: u32 = 120;

/// One step of a scripted play session.
#[derive(Clone, Copy)]
enum Step {
    /// Advance `n` frames with no input.
    Run(u32),
    /// Hold a button for `n` frames, then release it.
    Hold(Button, u32),
    /// Snapshot the framebuffer. With `moving`, it must differ from the
    /// previous snapshot (the game animates without input).
    Shot { label: &'static str, moving: bool },
}

/// Title -> Start -> snake runs on its own -> steering changes its path.
///
/// Validated against headless PyBoy. No RAM addresses are used, so the script
/// survives rebuilding the ROM.
const PLAY_SNAKE: &[Step] = &[
    Step::Run(PLAY_BOOT_FRAMES),
    Step::Shot {
        label: "title",
        moving: false,
    },
    Step::Hold(Button::Start, 5),
    Step::Run(55),
    Step::Shot {
        label: "Start leaves the title screen",
        moving: false,
    },
    // The snake auto-advances every <= 14 frames: 30 frames is >= 2 steps.
    Step::Run(30),
    Step::Shot {
        label: "snake moves on its own",
        moving: true,
    },
    Step::Hold(Button::Down, 3),
    Step::Run(60),
    Step::Shot {
        label: "Down steers the snake",
        moving: false,
    },
];

/// Title -> Start -> serve -> A launches the ball -> Right moves the paddle.
const PLAY_BREAKOUT: &[Step] = &[
    Step::Run(PLAY_BOOT_FRAMES),
    Step::Shot {
        label: "title",
        moving: false,
    },
    Step::Hold(Button::Start, 5),
    Step::Run(55),
    Step::Shot {
        label: "Start leaves the title screen",
        moving: false,
    },
    Step::Hold(Button::A, 3),
    Step::Run(30),
    Step::Shot {
        label: "A launches the ball",
        moving: false,
    },
    Step::Hold(Button::Right, 30),
    Step::Shot {
        label: "Right moves the paddle",
        moving: false,
    },
];

/// Play `script` on a fresh machine; returns the snapshot of every `Shot`.
/// `skip` replaces the `Hold` at that index with an equally long `Run`.
fn play(
    rom: &[u8],
    script: &[Step],
    skip: Option<usize>,
    max_frames: u32,
) -> Result<Vec<Vec<u8>>, Outcome> {
    let mut gb =
        GameBoy::new(rom.to_vec()).map_err(|e| Outcome::Fail(format!("load error: {e}")))?;
    let mut frame = 0;
    let mut shots = Vec::new();
    for (i, step) in script.iter().enumerate() {
        let (button, n) = match *step {
            Step::Shot { .. } => {
                shots.push(gb.framebuffer().to_vec());
                continue;
            }
            Step::Hold(_, n) if skip == Some(i) => (None, n),
            Step::Hold(b, n) => (Some(b), n),
            Step::Run(n) => (None, n),
        };
        if let Some(b) = button {
            gb.set_button(b, true);
        }
        for _ in 0..n {
            if frame >= max_frames {
                return Err(Outcome::Timeout(format!(
                    "play script needs more than {max_frames} frames"
                )));
            }
            gb.step_frame();
            frame += 1;
            if gb.cpu_locked() {
                return Err(Outcome::Fail(locked_message(&gb)));
            }
        }
        if let Some(b) = button {
            gb.set_button(b, false);
        }
    }
    Ok(shots)
}

/// Scripted playability check.
///
/// * The first snapshot (title screen) must not be a single shade.
/// * `Shot { moving: true }` must differ from the previous snapshot.
/// * Every `Hold` must matter: a control run with that press left out (the
///   emulator is deterministic, so everything else is identical) must produce a
///   different picture at the next `Shot`. Without this, a game that animates
///   on its own would pass even with joypad input broken.
fn run_play(rom: &[u8], script: &[Step], max_frames: u32) -> Outcome {
    let shots = match play(rom, script, None, max_frames) {
        Ok(s) => s,
        Err(o) => return o,
    };
    if shots.first().is_none_or(|t| t.iter().all(|&x| x == t[0])) {
        return Outcome::Fail("title screen is blank".into());
    }
    let mut shot_idx = 0;
    for (i, step) in script.iter().enumerate() {
        match *step {
            Step::Shot { label, moving } => {
                if moving && shots[shot_idx] == shots[shot_idx - 1] {
                    return Outcome::Fail(format!("{label}: framebuffer did not change"));
                }
                shot_idx += 1;
            }
            Step::Hold(..) => {
                let Some((next, label)) = script[i..]
                    .iter()
                    .filter_map(|s| match s {
                        Step::Shot { label, .. } => Some(*label),
                        _ => None,
                    })
                    .enumerate()
                    .map(|(k, l)| (shot_idx + k, l))
                    .next()
                else {
                    continue;
                };
                let control = match play(rom, script, Some(i), max_frames) {
                    Ok(s) => s,
                    Err(o) => return o,
                };
                if control[next] == shots[next] {
                    return Outcome::Fail(format!(
                        "{label}: input had no effect (same picture without it)"
                    ));
                }
            }
            Step::Run(_) => {}
        }
    }
    Outcome::Pass
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_and_paths_with_spaces_survive() {
        let cases = load_manifest();
        assert!(cases.len() > 50);
        assert!(cases.iter().any(|c| c.path.ends_with("03-op sp,hl.gb")));
    }

    #[test]
    fn missing_rom_reports_missing() {
        let case = Case {
            path: "does/not/exist.gb".into(),
            kind: Kind::Blargg,
            expect_pass: false,
            max_frames: 1,
        };
        assert_eq!(run_case(&case), Outcome::Missing);
    }
}
