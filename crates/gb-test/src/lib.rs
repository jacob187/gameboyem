//! Headless test-ROM harness for `gb-core`.
//!
//! Never opens a window: runs on macOS, in Docker and in CI. The list of ROMs and
//! their expected outcomes lives in `expected.txt` (see its header).
//!
//! ROMs are looked up under `$GB_ROMS_DIR` if set, else `<workspace>/roms`.

use std::fmt;
use std::path::{Path, PathBuf};

use gb_core::{Button, GameBoy, Model, SCREEN_HEIGHT, SCREEN_WIDTH};

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
    /// dmg-acid2: framebuffer compared to the reference image. With the `-cgb`
    /// suffix (cgb-acid2) the RGB555 colour framebuffer is compared instead.
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
    /// Corpus: Tobu Tobu Girl (MBC1). Intro, title, menu, then steer the girl.
    PlayTobu,
    /// Corpus: Adjustris (MBC3). Start, then move and drop the falling piece.
    PlayAdjustris,
    /// Corpus: Renegade Rush (MBC5). Title, menu, A starts a race, steer the car.
    PlayRenegade,
    /// Homebrew sound: the title screen is silent, and Start and the first eat
    /// (then game over) of snake produce audio that is absent without the input.
    SfxSnake,
    /// Same for breakout: Start, paddle hit, brick break, life lost, game over.
    SfxBreakout,
}

#[derive(Clone, Debug)]
pub struct Case {
    /// Path relative to the ROMs directory.
    pub path: String,
    pub kind: Kind,
    pub expect_pass: bool,
    pub max_frames: u32,
    /// The console to run on: a `-cgb` suffix on the kind selects [`Model::Cgb`].
    pub model: Model,
}

impl Case {
    /// Short display name, e.g. `blargg/cpu_instrs/individual/01-special`.
    pub fn name(&self) -> String {
        let p = self.path.strip_prefix("test/").unwrap_or(&self.path);
        let p = p
            .strip_suffix(".gbc")
            .or_else(|| p.strip_suffix(".gb"))
            .unwrap_or(p);
        match self.kind {
            // Same ROM as the smoke case; keep the names unique.
            Kind::PlaySnake
            | Kind::PlayBreakout
            | Kind::PlayTobu
            | Kind::PlayAdjustris
            | Kind::PlayRenegade => format!("{p}-play"),
            Kind::SaveSnake | Kind::SaveBreakout => format!("{p}-save"),
            Kind::SfxSnake | Kind::SfxBreakout => format!("{p}-sfx"),
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
            // `<kind>-cgb` runs <kind> on a CGB. Only the `-cgb` suffix is peeled
            // off, so `blargg-mem` and friends keep parsing.
            let kind_text = next("kind");
            let (kind_text, model) = match kind_text.strip_suffix("-cgb") {
                Some(base) => (base, Model::Cgb),
                None => (kind_text, Model::Dmg),
            };
            let kind = match kind_text {
                "blargg" => Kind::Blargg,
                "blargg-mem" => Kind::BlarggMem,
                "mooneye" => Kind::Mooneye,
                "acid2" => Kind::Acid2,
                "smoke" => Kind::Smoke,
                "play-snake" => Kind::PlaySnake,
                "play-breakout" => Kind::PlayBreakout,
                "save-snake" => Kind::SaveSnake,
                "save-breakout" => Kind::SaveBreakout,
                "play-tobu" => Kind::PlayTobu,
                "play-adjustris" => Kind::PlayAdjustris,
                "play-renegade" => Kind::PlayRenegade,
                "sfx-snake" => Kind::SfxSnake,
                "sfx-breakout" => Kind::SfxBreakout,
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
                model,
            }
        })
        .collect()
}

/// Run one case to completion (or its frame limit).
///
/// Kinds that script their own machines (play, save, sfx, blargg-mem with its
/// reset protocol) take the ROM bytes; the rest run on one booted `GameBoy`.
pub fn run_case(case: &Case) -> Outcome {
    let path = case.rom_path();
    let Ok(rom) = std::fs::read(&path) else {
        return Outcome::Missing;
    };
    let frames = case.max_frames;
    let model = case.model;
    let on_machine =
        |run: &dyn Fn(&mut GameBoy) -> Outcome| match GameBoy::with_model(rom.clone(), model) {
            Ok(mut gb) => run(&mut gb),
            Err(e) => Outcome::Fail(format!("load error: {e}")),
        };
    match case.kind {
        Kind::Blargg => on_machine(&|gb| run_blargg(gb, frames)),
        Kind::Mooneye => on_machine(&|gb| run_mooneye(gb, frames)),
        Kind::Acid2 => on_machine(&|gb| run_acid2(gb, case)),
        Kind::Smoke => on_machine(&|gb| run_smoke(gb, frames)),
        Kind::BlarggMem => run_blargg_mem(&rom, model, frames),
        Kind::PlaySnake => run_play(&rom, model, PLAY_SNAKE, frames),
        Kind::PlayBreakout => run_play(&rom, model, PLAY_BREAKOUT, frames),
        Kind::PlayTobu => run_play(&rom, model, PLAY_TOBU, frames),
        Kind::PlayAdjustris => run_play(&rom, model, PLAY_ADJUSTRIS, frames),
        Kind::PlayRenegade => run_play(&rom, model, PLAY_RENEGADE, frames),
        Kind::SaveSnake => run_save(&rom, model, drive_snake, frames),
        Kind::SaveBreakout => run_save(&rom, model, drive_breakout, frames),
        Kind::SfxSnake => run_sfx(&rom, model, sfx_snake, frames),
        Kind::SfxBreakout => run_sfx(&rom, model, sfx_breakout, frames),
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

/// Cartridge RAM as the ROM left it, read around the MBC's RAM-enable gate.
///
/// `GameBoy::peek` goes through the gate and returns 0xFF once a ROM has
/// disabled SRAM again, so a ROM that does so after writing its result would
/// time out. `battery_ram()` hands back the raw contents (RAM bank 0 starts at
/// offset 0 on every mapper, which is the bank Blargg's ROMs use). It only
/// exists for battery-backed carts; for any other cart this falls back to the
/// gated `peek`, which is a known limitation (gb-core has no ungated read).
struct Sram(Option<Vec<u8>>);

impl Sram {
    fn of(gb: &GameBoy) -> Self {
        Sram(gb.battery_ram())
    }

    fn read(&self, gb: &GameBoy, addr: u16) -> u8 {
        match &self.0 {
            Some(ram) => ram
                .get(usize::from(addr.wrapping_sub(0xA000)))
                .copied()
                .unwrap_or(0xFF),
            None => gb.peek(addr),
        }
    }
}

/// Blargg's memory protocol: `DE B0 61` at A001-A003 marks the block valid,
/// A000 holds 0x80 while running and then the result code (0 = pass, 0x81 =
/// "needs reset"), and a NUL-terminated report starts at A004.
///
/// On 0x81 the ROM wants a reset after >= 0.1 s with its SRAM kept (it is
/// battery-backed on real carts): the machine is re-created from the ROM, the
/// RAM carried over, and the run continues, at most `MAX_RESETS` times.
fn run_blargg_mem(rom: &[u8], model: Model, max_frames: u32) -> Outcome {
    const STATUS: u16 = 0xA000;
    const TEXT: u16 = 0xA004;
    const RUNNING: u8 = 0x80;
    const NEEDS_RESET: u8 = 0x81;
    const MAX_RESETS: u32 = 5;
    /// Frames to wait before resetting (>= 0.1 s).
    const RESET_DELAY: u32 = 7;
    /// Frames after a reset during which a leftover 0x81 is not a new request:
    /// the ROM has not rewritten the status byte yet.
    const BOOT_GRACE: u32 = 30;
    let boot = || {
        GameBoy::with_model(rom.to_vec(), model)
            .map_err(|e| Outcome::Fail(format!("load error: {e}")))
    };
    let report = |gb: &GameBoy, sram: &Sram| {
        let text: Vec<u8> = (0..0x1000u16)
            .map(|i| sram.read(gb, TEXT + i))
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
    let mut gb = match boot() {
        Ok(gb) => gb,
        Err(o) => return o,
    };
    let mut resets = 0;
    let mut grace_until = 0;
    for frame in 0..max_frames {
        gb.step_frame();
        let sram = Sram::of(&gb);
        if gb.cpu_locked() {
            return Outcome::Fail(format!(
                "{}; report: {}",
                locked_message(&gb),
                report(&gb, &sram)
            ));
        }
        let signed = [0xDE, 0xB0, 0x61]
            .iter()
            .enumerate()
            .all(|(i, &b)| sram.read(&gb, STATUS + 1 + i as u16) == b);
        if !signed {
            continue;
        }
        match sram.read(&gb, STATUS) {
            RUNNING => {}
            NEEDS_RESET if frame < grace_until => {}
            NEEDS_RESET => {
                if resets == MAX_RESETS {
                    return Outcome::Fail(format!("needs reset more than {MAX_RESETS} times"));
                }
                resets += 1;
                for _ in 0..RESET_DELAY {
                    gb.step_frame();
                }
                let saved = Sram::of(&gb).0;
                gb = match boot() {
                    Ok(gb) => gb,
                    Err(o) => return o,
                };
                if let Some(ram) = saved {
                    if let Err(e) = gb.load_battery_ram(&ram) {
                        return Outcome::Fail(format!("carrying SRAM across reset: {e}"));
                    }
                }
                grace_until = frame + BOOT_GRACE;
            }
            0 => return Outcome::Pass,
            code => return Outcome::Fail(format!("code {code:#04X}: {}", report(&gb, &sram))),
        }
    }
    let sram = Sram::of(&gb);
    Outcome::Timeout(format!(
        "status {:#04X}: {}",
        sram.read(&gb, STATUS),
        report(&gb, &sram)
    ))
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

/// Load a 160x144 reference PNG as RGB888. Indexed (palette), greyscale and
/// RGB(A) images are all expanded; alpha is dropped.
pub fn load_reference_rgb(path: &Path) -> Result<Vec<[u8; 3]>, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    decode_reference_rgb(std::io::BufReader::new(file))
}

fn decode_reference_rgb(
    src: impl std::io::BufRead + std::io::Seek,
) -> Result<Vec<[u8; 3]>, String> {
    let mut decoder = png::Decoder::new(src);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size().ok_or("png too large")?];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    if info.width as usize != SCREEN_WIDTH || info.height as usize != SCREEN_HEIGHT {
        return Err(format!("reference is {}x{}", info.width, info.height));
    }
    let channels = info.color_type.samples();
    let to_rgb: fn(&[u8]) -> [u8; 3] = match info.color_type {
        png::ColorType::Grayscale | png::ColorType::GrayscaleAlpha => |px| [px[0]; 3],
        png::ColorType::Rgb | png::ColorType::Rgba => |px| [px[0], px[1], px[2]],
        png::ColorType::Indexed => return Err("palette was not expanded".into()),
    };
    Ok(buf[..info.buffer_size()]
        .chunks_exact(channels)
        .map(to_rgb)
        .collect())
}

/// Write an RGB888 framebuffer as a PNG (for debugging failed comparisons).
pub fn save_rgb_png(fb: &[[u8; 3]], path: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(
        std::io::BufWriter::new(file),
        SCREEN_WIDTH as u32,
        SCREEN_HEIGHT as u32,
    );
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let rgb: Vec<u8> = fb.iter().flatten().copied().collect();
    enc.write_header()
        .and_then(|mut w| w.write_image_data(&rgb))
        .map_err(|e| e.to_string())
}

/// Where failing-test artifacts (actual framebuffers) are written.
pub fn artifact_dir() -> PathBuf {
    workspace_root().join("target").join("gb-test-out")
}

fn run_acid2(gb: &mut GameBoy, case: &Case) -> Outcome {
    if case.model == Model::Cgb {
        return run_acid2_cgb(gb, case);
    }
    let reference_path = case.rom_path().with_file_name("reference-dmg.png");
    let reference = match load_reference(&reference_path) {
        Ok(r) => r,
        Err(e) => return Outcome::Fail(format!("reference image: {e}")),
    };
    if let Some(fail) = run_to_acid2_signal(gb, case.max_frames) {
        return fail;
    }
    let fb = gb.framebuffer();
    let diff = fb.iter().zip(&reference).filter(|(a, b)| a != b).count();
    if diff == 0 {
        Outcome::Pass
    } else {
        let out = acid2_failure_path(case);
        let saved = save_framebuffer_png(fb, &out)
            .map(|()| format!("; actual frame written to {}", out.display()))
            .unwrap_or_default();
        Outcome::Fail(format!("{diff} pixels differ from reference{saved}"))
    }
}

/// The test finishes setup within a few frames and signals with LD B,B;
/// render a couple more frames after it (or stop at the frame limit).
/// Returns the failure if the CPU locks up.
fn run_to_acid2_signal(gb: &mut GameBoy, max_frames: u32) -> Option<Outcome> {
    let mut frames_after_signal = None;
    for _ in 0..max_frames {
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
            return Some(Outcome::Fail(locked_message(gb)));
        }
    }
    None
}

fn acid2_failure_path(case: &Case) -> PathBuf {
    artifact_dir().join(format!("{}.png", case.name().replace('/', "_")))
}

/// cgb-acid2: the RGB555 framebuffer expanded to RGB888 (no colour correction)
/// must equal `reference.png` exactly.
fn run_acid2_cgb(gb: &mut GameBoy, case: &Case) -> Outcome {
    let reference_path = case.rom_path().with_file_name("reference.png");
    let reference = match load_reference_rgb(&reference_path) {
        Ok(r) => r,
        Err(e) => return Outcome::Fail(format!("reference image: {e}")),
    };
    if let Some(fail) = run_to_acid2_signal(gb, case.max_frames) {
        return fail;
    }
    let Some(fb) = gb.cgb_framebuffer() else {
        return Outcome::Fail(
            "cgb_framebuffer() is None: the machine is not rendering in CGB colour mode".into(),
        );
    };
    let actual: Vec<[u8; 3]> = fb.iter().map(|&px| gb_core::rgb555_to_rgb888(px)).collect();
    let diff = actual
        .iter()
        .zip(&reference)
        .filter(|(a, b)| a != b)
        .count();
    if diff == 0 {
        Outcome::Pass
    } else {
        let out = acid2_failure_path(case);
        let saved = save_rgb_png(&actual, &out)
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

/// Tobu Tobu Girl (MIT, MBC1+RAM+BATTERY). The Tangram/potato-tan intro runs
/// ~700 frames; Start then walks title -> main menu -> level, and Right/Left
/// move the girl. Validated against headless PyBoy (which boots through the
/// boot ROM, so the intro ends a little later than here; 900 frames is past it
/// in both).
const PLAY_TOBU: &[Step] = &[
    Step::Run(900),
    Step::Shot {
        label: "intro",
        moving: false,
    },
    Step::Hold(Button::Start, 5),
    Step::Run(60),
    Step::Shot {
        label: "Start skips the intro to the title",
        moving: false,
    },
    Step::Hold(Button::Start, 5),
    Step::Run(120),
    Step::Shot {
        label: "Start opens the main menu",
        moving: false,
    },
    Step::Hold(Button::Start, 5),
    Step::Run(180),
    Step::Shot {
        label: "Start begins the level",
        moving: false,
    },
    Step::Hold(Button::Right, 20),
    Step::Run(10),
    Step::Shot {
        label: "Right moves the girl",
        moving: false,
    },
    Step::Hold(Button::Left, 20),
    Step::Run(10),
    Step::Shot {
        label: "Left moves the girl",
        moving: false,
    },
];

/// Adjustris (CC0, MBC3+RAM+BATTERY). The piece type depends on timing, so the
/// script only relies on D-pad moves, which work for every piece.
const PLAY_ADJUSTRIS: &[Step] = &[
    Step::Run(120),
    Step::Shot {
        label: "title",
        moving: false,
    },
    Step::Hold(Button::Start, 5),
    Step::Run(60),
    Step::Shot {
        label: "Start begins the game",
        moving: false,
    },
    Step::Hold(Button::Left, 12),
    Step::Run(5),
    Step::Shot {
        label: "Left moves the piece",
        moving: false,
    },
    Step::Hold(Button::Right, 12),
    Step::Run(5),
    Step::Shot {
        label: "Right moves the piece",
        moving: false,
    },
    Step::Hold(Button::Down, 30),
    Step::Run(5),
    Step::Shot {
        label: "Down drops the piece",
        moving: false,
    },
];

/// Renegade Rush (MIT, MBC5+RAM+BATTERY). The Start press needs ~100 frames to
/// reach the menu; A on PLAY starts a race; Left/Right steer the car.
const PLAY_RENEGADE: &[Step] = &[
    Step::Run(200),
    Step::Shot {
        label: "title",
        moving: false,
    },
    Step::Hold(Button::Start, 5),
    Step::Run(120),
    Step::Shot {
        label: "Start opens the menu",
        moving: false,
    },
    Step::Hold(Button::A, 3),
    Step::Run(240),
    Step::Shot {
        label: "A starts the race",
        moving: false,
    },
    Step::Hold(Button::Left, 30),
    Step::Run(5),
    Step::Shot {
        label: "Left steers the car",
        moving: false,
    },
    Step::Hold(Button::Right, 60),
    Step::Run(5),
    Step::Shot {
        label: "Right steers the car",
        moving: false,
    },
];

/// Play `script` on a fresh machine; returns the snapshot of every `Shot`.
/// `skip` replaces the `Hold` at that index with an equally long `Run`.
fn play(
    rom: &[u8],
    model: Model,
    script: &[Step],
    skip: Option<usize>,
    max_frames: u32,
) -> Result<Vec<Vec<u8>>, Outcome> {
    let mut gb = GameBoy::with_model(rom.to_vec(), model)
        .map_err(|e| Outcome::Fail(format!("load error: {e}")))?;
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
///   different picture at the next `Shot` (the control run stops there; the
///   machine is not `Clone`, so each one replays from power-on). Without this, a game that animates
///   on its own would pass even with joypad input broken.
fn run_play(rom: &[u8], model: Model, script: &[Step], max_frames: u32) -> Outcome {
    let shots = match play(rom, model, script, None, max_frames) {
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
                if moving && shot_idx > 0 && shots[shot_idx] == shots[shot_idx - 1] {
                    return Outcome::Fail(format!("{label}: framebuffer did not change"));
                }
                shot_idx += 1;
            }
            Step::Hold(..) => {
                // The control run only has to reach the next snapshot.
                let Some((end, label)) =
                    script
                        .iter()
                        .enumerate()
                        .skip(i)
                        .find_map(|(j, s)| match s {
                            Step::Shot { label, .. } => Some((j, *label)),
                            _ => None,
                        })
                else {
                    continue;
                };
                let control = match play(rom, model, &script[..=end], Some(i), max_frames) {
                    Ok(s) => s,
                    Err(o) => return o,
                };
                if control[shot_idx] == shots[shot_idx] {
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
    fn new(rom: &[u8], model: Model, max_frames: u32) -> Result<Self, Outcome> {
        let gb = GameBoy::with_model(rom.to_vec(), model)
            .map_err(|e| Outcome::Fail(format!("load error: {e}")))?;
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
fn run_save(rom: &[u8], model: Model, bot: Bot, max_frames: u32) -> Outcome {
    match save_round_trip(rom, model, bot, max_frames) {
        Ok(()) => Outcome::Pass,
        Err(o) => o,
    }
}

fn save_round_trip(rom: &[u8], model: Model, bot: Bot, max_frames: u32) -> Result<(), Outcome> {
    let fail = |m: &str| Outcome::Fail(m.to_string());
    let mut first = Session::new(rom, model, max_frames)?;
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

    let mut second = Session::new(rom, model, max_frames)?;
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

// ---- Homebrew sound effects -------------------------------------------------

/// Peak-to-peak amplitude of a sample buffer. Insensitive to a DC offset, so it
/// keeps working if the APU output becomes bipolar / high-pass filtered.
fn peak_to_peak(samples: &[f32]) -> f32 {
    let (lo, hi) = samples
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), &x| (lo.min(x), hi.max(x)));
    if samples.is_empty() { 0.0 } else { hi - lo }
}

/// A window with at least this peak-to-peak level counts as sound ...
const SFX_LOUD: f32 = 0.02;
/// ... and one below this as silence.
const SFX_QUIET: f32 = 0.005;

/// A [`Session`] that keeps the audio of every frame.
struct Listener {
    s: Session,
    frames: Vec<Vec<f32>>,
}

impl Listener {
    fn new(rom: &[u8], model: Model, max_frames: u32) -> Result<Self, Outcome> {
        Ok(Self {
            s: Session::new(rom, model, max_frames)?,
            frames: Vec::new(),
        })
    }

    fn step(&mut self) -> Result<(), Outcome> {
        self.s.step()?;
        self.frames.push(self.s.gb.audio_samples());
        Ok(())
    }

    fn run(&mut self, n: u32) -> Result<(), Outcome> {
        (0..n).try_for_each(|_| self.step())
    }

    fn hold(&mut self, button: Button, n: u32) -> Result<(), Outcome> {
        self.s.gb.set_button(button, true);
        let r = self.run(n);
        self.s.gb.set_button(button, false);
        r
    }

    /// Frames run so far (the index of the next one).
    fn now(&self) -> usize {
        self.frames.len()
    }

    /// Peak-to-peak level over frames `[from, to)`, clamped to what was run.
    fn level(&self, from: usize, to: usize) -> f32 {
        let to = to.min(self.frames.len());
        let from = from.min(to);
        peak_to_peak(&self.frames[from..to].concat())
    }
}

/// Frames a one-shot effect gets: Start blip 78 ms and eat 50 ms fit in 5.
const SFX_WINDOW: usize = 8;

/// Checks one named window of a run against its control (or against silence).
struct SfxReport {
    lines: Vec<String>,
    failure: Option<String>,
}

impl SfxReport {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            failure: None,
        }
    }

    fn note(&mut self, what: &str, level: f32, want_loud: bool) {
        let ok = if want_loud {
            level >= SFX_LOUD
        } else {
            level <= SFX_QUIET
        };
        self.lines.push(format!(
            "{what}: {level:.4} ({})",
            if want_loud { "loud" } else { "quiet" }
        ));
        if !ok && self.failure.is_none() {
            self.failure = Some(format!(
                "{what}: peak-to-peak {level:.4}, expected {} {}",
                if want_loud { ">=" } else { "<=" },
                if want_loud { SFX_LOUD } else { SFX_QUIET }
            ));
        }
    }

    fn loud(&mut self, what: &str, level: f32) {
        self.note(what, level, true);
    }

    fn quiet(&mut self, what: &str, level: f32) {
        self.note(what, level, false);
    }

    fn finish(self) -> Outcome {
        if std::env::var_os("GB_SFX_LOG").is_some() {
            eprintln!("{}", self.lines.join("\n"));
        }
        match self.failure {
            None => Outcome::Pass,
            Some(f) => Outcome::Fail(f),
        }
    }
}

type SfxScript = fn(&[u8], Model, u32, &mut SfxReport) -> Result<(), Outcome>;

fn run_sfx(rom: &[u8], model: Model, script: SfxScript, max_frames: u32) -> Outcome {
    let mut report = SfxReport::new();
    match script(rom, model, max_frames, &mut report) {
        Ok(()) => report.finish(),
        Err(o) => o,
    }
}

/// The title screen is silent (skipping the first frames while the machine
/// settles), then Start gives a blip that a run without the press does not.
/// Leaves `l` on the first frame after the blip window, with the game running
/// (snake: field up, breakout: serving).
fn sfx_title_and_start(
    rom: &[u8],
    model: Model,
    l: &mut Listener,
    max_frames: u32,
    r: &mut SfxReport,
) -> Result<(), Outcome> {
    l.run(PLAY_BOOT_FRAMES)?;
    r.quiet("title", l.level(30, PLAY_BOOT_FRAMES as usize));
    l.hold(Button::Start, 5)?;
    l.run(SFX_WINDOW as u32 - 5)?;
    let end = PLAY_BOOT_FRAMES as usize + SFX_WINDOW;
    r.loud("Start blip", l.level(PLAY_BOOT_FRAMES as usize, end));

    let mut ctl = Listener::new(rom, model, max_frames)?;
    ctl.run(end as u32)?;
    r.quiet("Start omitted", ctl.level(PLAY_BOOT_FRAMES as usize, end));

    l.run(55 - (SFX_WINDOW as u32 - 5))?;
    Ok(())
}

/// Snake: Start blip, silent movement, the first eat, then game over.
///
/// Controls: no Start press; and Start but no steering, over the eat window.
fn sfx_snake(rom: &[u8], model: Model, max_frames: u32, r: &mut SfxReport) -> Result<(), Outcome> {
    const HEAD: u8 = 130;
    const FOOD: u8 = 131;
    let pads = [Button::Up, Button::Right, Button::Down, Button::Left];
    let mut l = Listener::new(rom, model, max_frames)?;
    sfx_title_and_start(rom, model, &mut l, max_frames, r)?;
    let moving_from = l.now();

    let food0 =
        l.s.find_tile(FOOD)
            .ok_or_else(|| Outcome::Fail("no food on the board".into()))?;
    let mut dir = 1; // the snake starts heading right
    let eat = loop {
        let chunk = l.now();
        match (l.s.find_tile(HEAD), l.s.find_tile(FOOD)) {
            (Some(_), Some(f)) if f == food0 => {}
            _ => break chunk - 2, // eaten during the previous 2-frame chunk
        }
        let head = l.s.find_tile(HEAD).unwrap_or(food0);
        let want = match (head.0.cmp(&food0.0), head.1.cmp(&food0.1)) {
            (std::cmp::Ordering::Less, _) => 1,
            (std::cmp::Ordering::Greater, _) => 3,
            (_, std::cmp::Ordering::Less) => 2,
            _ => 0,
        };
        let next = if want % 2 == dir % 2 && want != dir {
            if head.1 > 0 { 0 } else { 2 }
        } else {
            want
        };
        if next != dir {
            l.s.gb.set_button(pads[dir], false);
            dir = next;
        }
        l.s.gb.set_button(pads[dir], true);
        l.run(2)?;
    };
    // Steering and moving make no sound of their own.
    r.quiet("snake moving", l.level(moving_from, eat.saturating_sub(1)));
    l.run(SFX_WINDOW as u32)?;
    let win = (eat.saturating_sub(1), eat + SFX_WINDOW);
    r.loud("eat", l.level(win.0, win.1));

    // Same timeline as the main run, but no steering: nothing is eaten.
    let mut pre = Listener::new(rom, model, max_frames)?;
    pre.run(PLAY_BOOT_FRAMES)?;
    pre.hold(Button::Start, 5)?;
    pre.run(win.1 as u32 - PLAY_BOOT_FRAMES - 5)?;
    r.quiet("eat, steering omitted", pre.level(win.0, win.1));

    // Run into a wall: the first loud frame after the eat is the game over.
    let wall = if dir % 2 == 1 { 0 } else { dir };
    l.s.gb.set_button(pads[dir], false);
    l.s.gb.set_button(pads[wall], true);
    while !l.s.has_text(5, 4, "GAME OVER") {
        l.step()?;
    }
    l.s.gb.set_button(pads[wall], false);
    let after = win.1;
    let onset = (after..l.now()).find(|&f| l.level(f, f + 1) >= SFX_LOUD);
    match onset {
        Some(f) => r.loud("game over", l.level(f, f + SFX_WINDOW)),
        None => r.loud("game over", 0.0),
    }
    Ok(())
}

/// Breakout: Start blip, silent serve, then a bot that tracks the ball (paddle
/// hits, brick breaks) and finally lets it drop (life lost, game over).
///
/// Controls: no Start press; and Start with Left held but A never pressed, so
/// the ball is never launched, over the whole play span.
fn sfx_breakout(
    rom: &[u8],
    model: Model,
    max_frames: u32,
    r: &mut SfxReport,
) -> Result<(), Outcome> {
    const LIFE: u8 = 132;
    let mut l = Listener::new(rom, model, max_frames)?;
    sfx_title_and_start(rom, model, &mut l, max_frames, r)?;
    let serving = (l.now() - 20, l.now());
    r.quiet("serving", l.level(serving.0, serving.1));

    let score = |l: &Listener| -> Vec<u8> { (6..11).map(|x| l.s.bg_tile(x, 0)).collect() };
    let lives = |l: &Listener| (17..20).filter(|&x| l.s.bg_tile(x, 0) == LIFE).count();
    let ball_y = |l: &Listener| l.s.gb.peek(0xFE0C);

    let launch = l.now();
    let (mut bricks, mut paddles, mut lost) = (Vec::new(), Vec::new(), Vec::new());
    let mut over = None;
    let (mut last_score, mut last_lives) = (score(&l), lives(&l));
    let (mut old_y, mut last_y) = (ball_y(&l), ball_y(&l)); // ball OAM y, 2 and 1 frames ago
    l.s.gb.set_button(Button::A, true);
    while !l.s.has_text(5, 4, "GAME OVER") {
        // Phase 1 tracks the ball until a brick and the paddle were both hit;
        // phase 2 parks the paddle in the corner and lets the lives run out.
        let tracking = bricks.is_empty() || paddles.is_empty();
        let (pad, ball) = (l.s.gb.peek(0xFE01), l.s.gb.peek(0xFE0D));
        let diff = i16::from(ball) - i16::from(pad) - 8;
        for (b, on) in [
            (Button::Left, !tracking || diff < -2),
            (Button::Right, tracking && diff > 2),
        ] {
            l.s.gb.set_button(b, on);
        }
        if l.now() > launch + 1500 {
            return Err(Outcome::Fail("breakout did not reach game over".into()));
        }
        l.step()?;
        let at = l.now() - 1;
        let (sc, lv, y) = (score(&l), lives(&l), ball_y(&l));
        if over.is_none() {
            if sc != last_score {
                bricks.push(at);
            }
            // A paddle hit puts the ball at exactly y = 146 and sends it back up;
            // a miss, a serve and a launch do not come down onto 146 and rise.
            if old_y < 146 && last_y == 146 && y < 146 {
                paddles.push(at - 1);
            }
            if lv < last_lives {
                if lv == 0 {
                    over = Some(at);
                } else {
                    lost.push(at);
                }
            }
        }
        (last_score, last_lives, old_y, last_y) = (sc, lv, last_y, y);
    }
    for b in [Button::A, Button::Left, Button::Right] {
        l.s.gb.set_button(b, false);
    }
    let worst = |l: &Listener, ev: &[usize]| {
        ev.iter()
            .map(|&f| l.level(f.saturating_sub(1), f + SFX_WINDOW))
            .fold(f32::MAX, f32::min)
    };
    for (name, ev) in [
        ("brick", &bricks),
        ("paddle", &paddles),
        ("life lost", &lost),
    ] {
        if ev.is_empty() {
            return Err(Outcome::Fail(format!("no {name} event happened")));
        }
        r.loud(&format!("{name} (worst of {})", ev.len()), worst(&l, ev));
    }
    let Some(over) = over else {
        return Err(Outcome::Fail("lives never reached zero".into()));
    };
    r.loud("game over", worst(&l, &[over]));

    let span = (launch, over + SFX_WINDOW);
    let mut ctl = Listener::new(rom, model, max_frames)?;
    ctl.run(PLAY_BOOT_FRAMES)?;
    ctl.hold(Button::Start, 5)?;
    ctl.s.gb.set_button(Button::Left, true);
    ctl.run((span.1 - ctl.now()) as u32)?;
    r.quiet("play span, A omitted", ctl.level(span.0, span.1));
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
    fn cgb_suffix_selects_the_model_and_only_the_suffix() {
        let cases = parse_manifest(
            "pass mooneye 10 a b.gb\n\
             fail mooneye-cgb 20 c d.gb\n\
             pass blargg-mem-cgb 30 e.gb\n\
             pass blargg-mem 40 f.gb\n\
             fail acid2-cgb 50 g.gbc\n",
        );
        let got: Vec<_> = cases
            .iter()
            .map(|c| (c.kind, c.model, c.max_frames))
            .collect();
        assert_eq!(
            got,
            [
                (Kind::Mooneye, Model::Dmg, 10),
                (Kind::Mooneye, Model::Cgb, 20),
                (Kind::BlarggMem, Model::Cgb, 30),
                (Kind::BlarggMem, Model::Dmg, 40),
                (Kind::Acid2, Model::Cgb, 50),
            ]
        );
        assert_eq!(cases[1].path, "c d.gb");
    }

    /// A 160x144 4-bit palette PNG (the cgb-acid2 reference format) in memory.
    fn indexed_png(width: u32, height: u32, palette: &[u8], indices: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Indexed);
        enc.set_depth(png::BitDepth::Four);
        enc.set_palette(palette.to_vec());
        let mut w = enc.write_header().unwrap();
        // Two 4-bit pixels per byte, rows byte-aligned.
        let row_bytes = (width as usize).div_ceil(2);
        let mut packed = vec![0u8; row_bytes * height as usize];
        for (i, &idx) in indices.iter().enumerate() {
            let (y, x) = (i / width as usize, i % width as usize);
            packed[y * row_bytes + x / 2] |= idx << (if x % 2 == 0 { 4 } else { 0 });
        }
        w.write_image_data(&packed).unwrap();
        w.finish().unwrap();
        out
    }

    #[test]
    fn indexed_reference_png_loads_as_rgb888() {
        let palette = [0xFF, 0x00, 0x00, 0x00, 0x80, 0x00, 0x12, 0x34, 0x56];
        let indices: Vec<u8> = (0..SCREEN_WIDTH * SCREEN_HEIGHT)
            .map(|i| (i % 3) as u8)
            .collect();
        let png = indexed_png(
            SCREEN_WIDTH as u32,
            SCREEN_HEIGHT as u32,
            &palette,
            &indices,
        );
        let px = decode_reference_rgb(std::io::Cursor::new(png)).unwrap();
        assert_eq!(px.len(), SCREEN_WIDTH * SCREEN_HEIGHT);
        assert_eq!(
            &px[..4],
            &[[255, 0, 0], [0, 128, 0], [0x12, 0x34, 0x56], [255, 0, 0]]
        );
        assert_eq!(px[SCREEN_WIDTH * SCREEN_HEIGHT - 1], {
            let i = (SCREEN_WIDTH * SCREEN_HEIGHT - 1) % 3;
            [palette[i * 3], palette[i * 3 + 1], palette[i * 3 + 2]]
        });
    }

    #[test]
    fn reference_png_of_the_wrong_size_is_rejected() {
        let png = indexed_png(4, 2, &[0, 0, 0, 255, 255, 255], &[0, 1, 0, 1, 1, 0, 1, 0]);
        let err = decode_reference_rgb(std::io::Cursor::new(png)).unwrap_err();
        assert!(err.contains("4x2"), "{err}");
    }

    #[test]
    fn gbc_and_gb_extensions_are_stripped_from_names() {
        let case = |path: &str| Case {
            path: path.into(),
            kind: Kind::Acid2,
            expect_pass: true,
            max_frames: 1,
            model: Model::Cgb,
        };
        assert_eq!(
            case("test/cgb-acid2/cgb-acid2.gbc").name(),
            "cgb-acid2/cgb-acid2"
        );
        assert_eq!(
            case("test/dmg-acid2/dmg-acid2.gb").name(),
            "dmg-acid2/dmg-acid2"
        );
    }

    /// A 32 KiB ROM with `code` at 0x150 (entry jumps there).
    fn synth_rom(cart_type: u8, ram_code: u8, code: &[u8]) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x100..0x104].copy_from_slice(&[0x00, 0xC3, 0x50, 0x01]); // nop; jp $0150
        rom[0x147] = cart_type;
        rom[0x149] = ram_code;
        rom[0x150..0x150 + code.len()].copy_from_slice(code);
        rom
    }

    /// Shows one BG tile with a stripe and latches every D-pad press it sees
    /// into BGP (the latch survives the release, so a snapshot taken after the
    /// `Hold` still shows it). Buttons (A, B, ...) are never selected, so they
    /// change nothing.
    const JOYPAD_TO_PALETTE: &[u8] = &[
        0xF3, // di
        0xF0, 0x44, 0xFE, 0x90, 0x20, 0xFA, // wait for LY == 144
        0xAF, 0xE0, 0x40, // LCD off
        0x21, 0x00, 0x80, 0x01, 0x00, 0x20, // hl = $8000, bc = $2000
        0xAF, 0x22, 0x0B, 0x78, 0xB1, 0x20, 0xF9, // clear VRAM
        0x21, 0x00, 0x80, 0x36, 0xFF, // tile 0, first row = colour 1
        0x3E, 0x91, 0xE0, 0x40, // LCD on, BG on
        0x06, 0x00, // b = latch = 0
        // loop:
        0x3E, 0x20, 0xE0, 0x00, // select the D-pad
        0xF0, 0x00, 0xE6, 0x0C, 0xEE, 0x0C, // a = pressed D-pad bits 3:2
        0xB0, 0x47, // a |= b; b = a
        0xF6, 0x03, 0xE0, 0x47, // BGP = a | 3
        0x18, 0xEE, // jr loop
    ];

    fn play_script(button: Button) -> [Step; 5] {
        [
            Step::Run(20), // the boot code clears VRAM first (~1.2 frames)
            Step::Shot {
                label: "title",
                moving: false,
            },
            Step::Hold(button, 3),
            Step::Run(2),
            Step::Shot {
                label: "after the press",
                moving: false,
            },
        ]
    }

    #[test]
    fn play_control_run_passes_when_input_changes_the_picture() {
        let rom = synth_rom(0x00, 0, JOYPAD_TO_PALETTE);
        let out = run_play(&rom, Model::Dmg, &play_script(Button::Down), 100);
        assert_eq!(out, Outcome::Pass, "{out}");
    }

    #[test]
    fn play_control_run_fails_when_input_is_ignored() {
        let rom = synth_rom(0x00, 0, JOYPAD_TO_PALETTE);
        let out = run_play(&rom, Model::Dmg, &play_script(Button::A), 100);
        assert!(
            matches!(&out, Outcome::Fail(m) if m.contains("input had no effect")),
            "{out}"
        );
    }

    /// Cart type MBC1+RAM+BATTERY, 8 KiB RAM. Enables SRAM, signs the block,
    /// then: first run sets A010 and reports 0x81 (needs reset); the second
    /// run (A010 survived the reset) reports 0 and disables SRAM again.
    const BLARGG_RESET_THEN_PASS: &[u8] = &[
        0x3E, 0x0A, 0xEA, 0x00, 0x00, // enable SRAM
        0x3E, 0xDE, 0xEA, 0x01, 0xA0, //
        0x3E, 0xB0, 0xEA, 0x02, 0xA0, //
        0x3E, 0x61, 0xEA, 0x03, 0xA0, // signature
        0xFA, 0x10, 0xA0, 0xB7, 0x20, 0x0C, // A010 != 0 -> second run
        0x3E, 0x01, 0xEA, 0x10, 0xA0, // A010 = 1
        0x3E, 0x81, 0xEA, 0x00, 0xA0, // status = needs reset
        0x18, 0xFE, //
        0xAF, 0xEA, 0x00, 0xA0, // status = 0 (pass)
        0xEA, 0x00, 0x00, // disable SRAM
        0x18, 0xFE,
    ];

    #[test]
    fn blargg_mem_resets_on_0x81_and_reads_sram_ungated() {
        let rom = synth_rom(0x03, 0x02, BLARGG_RESET_THEN_PASS);
        assert_eq!(run_blargg_mem(&rom, Model::Dmg, 600), Outcome::Pass);
    }

    #[test]
    fn blargg_mem_gives_up_after_too_many_resets() {
        let mut code = BLARGG_RESET_THEN_PASS.to_vec();
        code[23..26].fill(0x00); // NOP out `or a; jr nz`: every run reports "needs reset"
        let rom = synth_rom(0x03, 0x02, &code);
        let out = run_blargg_mem(&rom, Model::Dmg, 3000);
        assert!(
            matches!(&out, Outcome::Fail(m) if m.contains("needs reset")),
            "{out}"
        );
    }

    #[test]
    fn missing_rom_reports_missing() {
        let case = Case {
            path: "does/not/exist.gb".into(),
            kind: Kind::Blargg,
            expect_pass: false,
            max_frames: 1,
            model: Model::Dmg,
        };
        assert_eq!(run_case(&case), Outcome::Missing);
    }
}
