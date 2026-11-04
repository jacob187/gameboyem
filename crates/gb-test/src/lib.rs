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
    /// Homebrew save: play snake to game over, carry its battery RAM into a fresh
    /// machine, and the title screen must show the saved high score.
    SaveSnake,
    /// Same as `SaveSnake` for breakout.
    SaveBreakout,
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
            Kind::SaveSnake | Kind::SaveBreakout => format!("{p}-save"),
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
                "save-snake" => Kind::SaveSnake,
                "save-breakout" => Kind::SaveBreakout,
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
        Kind::SaveSnake => return run_save(&rom, drive_snake, case.max_frames),
        Kind::SaveBreakout => return run_save(&rom, drive_breakout, case.max_frames),
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
        Kind::PlaySnake | Kind::PlayBreakout | Kind::SaveSnake | Kind::SaveBreakout => {
            unreachable!("handled above")
        }
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

/// A homebrew game is driven by a bot until its game-over screen is up.
type Bot = fn(&mut Session) -> Result<(), Outcome>;

/// A machine plus the frame budget shared by everything run on it.
struct Session {
    gb: GameBoy,
    frame: u32,
    max_frames: u32,
}

impl Session {
    fn new(rom: &[u8], max_frames: u32) -> Result<Self, Outcome> {
        let gb =
            GameBoy::new(rom.to_vec()).map_err(|e| Outcome::Fail(format!("load error: {e}")))?;
        Ok(Self {
            gb,
            frame: 0,
            max_frames,
        })
    }

    fn step(&mut self) -> Result<(), Outcome> {
        if self.frame >= self.max_frames {
            return Err(Outcome::Timeout(format!(
                "save script needs more than {} frames",
                self.max_frames
            )));
        }
        self.gb.step_frame();
        self.frame += 1;
        if self.gb.cpu_locked() {
            return Err(Outcome::Fail(locked_message(&self.gb)));
        }
        Ok(())
    }

    fn run(&mut self, n: u32) -> Result<(), Outcome> {
        (0..n).try_for_each(|_| self.step())
    }

    fn hold(&mut self, button: Button, n: u32) -> Result<(), Outcome> {
        self.gb.set_button(button, true);
        let r = self.run(n);
        self.gb.set_button(button, false);
        r
    }

    /// Background tile id at tile position (x, y) of the 0x9800 map.
    fn bg_tile(&self, x: u16, y: u16) -> u8 {
        self.gb.peek(0x9800 + y * 32 + x)
    }

    /// First tile position holding `tile`.
    fn find_tile(&self, tile: u8) -> Option<(u16, u16)> {
        (0..18)
            .flat_map(|y| (0..20).map(move |x| (x, y)))
            .find(|&(x, y)| self.bg_tile(x, y) == tile)
    }

    /// True when `text` is printed at tile position (x, y); GBDK's font puts
    /// ASCII `c` at tile `c - 32`.
    fn has_text(&self, x: u16, y: u16, text: &str) -> bool {
        text.bytes()
            .zip(x..)
            .all(|(c, x)| self.bg_tile(x, y) == c - 32)
    }

    /// Run until `text` is on screen at (x, y).
    fn run_until_text(&mut self, x: u16, y: u16, text: &str) -> Result<(), Outcome> {
        while !self.has_text(x, y, text) {
            self.step()?;
        }
        Ok(())
    }
}

/// Snake: Start, steer to the first food (read from the tile map), then hold a
/// direction into a wall until GAME OVER. Tile ids are snake's `TILE_BASE + 2/3`.
fn drive_snake(s: &mut Session) -> Result<(), Outcome> {
    const HEAD: u8 = 130;
    const FOOD: u8 = 131;
    let pads = [Button::Up, Button::Right, Button::Down, Button::Left];
    s.hold(Button::Start, 5)?;
    s.run(55)?;
    let food0 = s
        .find_tile(FOOD)
        .ok_or_else(|| Outcome::Fail("no food on the board".into()))?;
    let mut dir = 1; // index into `pads`: the snake starts heading right
    loop {
        match (s.find_tile(HEAD), s.find_tile(FOOD)) {
            (Some(_), Some(f)) if f == food0 => {}
            _ => break, // the first food was eaten (a new one appeared elsewhere)
        }
        let head = s.find_tile(HEAD).unwrap_or(food0);
        let want = match (head.0.cmp(&food0.0), head.1.cmp(&food0.1)) {
            (std::cmp::Ordering::Less, _) => 1,
            (std::cmp::Ordering::Greater, _) => 3,
            (_, std::cmp::Ordering::Less) => 2,
            _ => 0,
        };
        // A 180 degree turn is ignored by the game; sidestep instead.
        let next = if want % 2 == dir % 2 && want != dir {
            if head.1 > 0 { 0 } else { 2 }
        } else {
            want
        };
        if next != dir {
            s.gb.set_button(pads[dir], false);
            dir = next;
        }
        s.gb.set_button(pads[dir], true);
        s.run(2)?;
    }
    // Keep going vertically until the snake hits something.
    let wall = if dir % 2 == 1 { 0 } else { dir };
    s.gb.set_button(pads[dir], false);
    s.gb.set_button(pads[wall], true);
    s.run_until_text(5, 4, "GAME OVER")?;
    s.gb.set_button(pads[wall], false);
    s.run(10)
}

/// Breakout: Start, then hold Left+A. The paddle sits in the corner and A
/// (re)serves every ball, which hits a brick on the way up and is then lost;
/// three lives later the GAME OVER screen is up.
fn drive_breakout(s: &mut Session) -> Result<(), Outcome> {
    s.hold(Button::Start, 5)?;
    s.run(55)?;
    s.gb.set_button(Button::Left, true);
    s.gb.set_button(Button::A, true);
    s.run_until_text(5, 4, "GAME OVER")?;
    s.gb.set_button(Button::Left, false);
    s.gb.set_button(Button::A, false);
    s.run(10)
}

/// Battery-RAM round trip, validated against headless PyBoy.
///
/// 1. Boot a fresh machine; its title screen is the control (no saved score).
/// 2. `bot` plays a game that scores and ends in GAME OVER.
/// 3. The battery RAM must be non-empty and loadable into a second fresh
///    machine, and read back unchanged.
/// 4. That machine's title screen (after the same boot time) must differ from
///    the control: the HI digits show the saved score.
///
/// No RAM addresses or save format are assumed; only that something persists.
fn run_save(rom: &[u8], bot: Bot, max_frames: u32) -> Outcome {
    match save_round_trip(rom, bot, max_frames) {
        Ok(()) => Outcome::Pass,
        Err(o) => o,
    }
}

fn save_round_trip(rom: &[u8], bot: Bot, max_frames: u32) -> Result<(), Outcome> {
    let fail = |m: &str| Outcome::Fail(m.to_string());
    let mut first = Session::new(rom, max_frames)?;
    first.run(PLAY_BOOT_FRAMES)?;
    let control = first.gb.framebuffer().to_vec();
    if control.iter().all(|&x| x == control[0]) {
        return Err(fail("title screen is blank"));
    }
    if first
        .gb
        .battery_ram()
        .is_none_or(|r| r.iter().any(|&b| b != 0))
    {
        return Err(fail("fresh cartridge RAM is missing or not zero"));
    }

    bot(&mut first)?;
    let Some(saved) = first.gb.battery_ram() else {
        return Err(fail(
            "cartridge has no battery RAM (check header 0x147/0x149)",
        ));
    };
    if saved.iter().all(|&b| b == 0) {
        return Err(fail(
            "game over reached but nothing was written to cartridge RAM",
        ));
    }

    let mut second = Session::new(rom, max_frames)?;
    second
        .gb
        .load_battery_ram(&saved)
        .map_err(|e| Outcome::Fail(format!("loading the save failed: {e}")))?;
    if second.gb.battery_ram().as_deref() != Some(&saved[..]) {
        return Err(fail("battery RAM did not read back as loaded"));
    }
    second.run(PLAY_BOOT_FRAMES)?;
    if second.gb.framebuffer() == &control[..] {
        return Err(fail(
            "title screen with a save equals the fresh one (no HI shown)",
        ));
    }
    Ok(())
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
