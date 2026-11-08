//! The winit application: window, input, frame pacing and presentation.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use emu_common::replay::{Event, Record};
use emu_common::{Button, Core, FrameRef, Pixels as FramePixels};
use gb_core::{Framebuffer, SCREEN_HEIGHT, SCREEN_WIDTH};
use pixels::Pixels;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

use crate::audio::{self, AudioOutput, ToneGen};
use crate::gui::{Gui, Screen};
use crate::keymap::{Action, InputState, map, menu_takes_key};
use crate::library::{GameId, GamePaths, Library, RemoveSaves};
use crate::pacer::{FramePacer, Steps, audio_seconds, run_due};
use crate::record::{self, Recorder};
use crate::save::SaveManager;
use crate::shutdown::ShutdownFlag;
use crate::state::{Aside, Resume, Slot, StateStore};
use crate::ui::{
    EscapeEffect, Mode, SnapshotItem, ThumbnailTrigger, Toasts, UiAction, escape_effect,
    frame_is_uniform, friendly_time_local, snapshot_items,
};
use crate::video::{Palette, present, test_pattern};

/// Speed multiplier while Tab is held.
const FAST_FORWARD_SPEED: f64 = 4.0;

/// How often the resume point is rewritten while a game is played, so a crash loses at most
/// this much.
const RESUME_INTERVAL: Duration = Duration::from_secs(60);

/// How long a toast that must be read stays (a resume point that could not be used).
const LONG_TOAST_DURATION: Duration = Duration::from_secs(8);

/// How long a snapshot message stays in the window title.
const STATUS_DURATION: Duration = Duration::from_secs(2);

/// The window's size, in logical pixels, while the library is shown.
const LIBRARY_WINDOW: (u32, u32) = (880, 640);

/// What a key press or a click asks of the event loop.
#[derive(Debug, PartialEq, Eq)]
enum Effect {
    None,
    /// Close the program (saves are written as the loop exits).
    Exit,
}

/// The size to configure the surface with, or `None` while the window has no
/// drawable area (minimised: a zero dimension makes wgpu panic or error).
fn surface_extent(width: u32, height: u32) -> Option<(u32, u32)> {
    (width > 0 && height > 0).then_some((width, height))
}

/// What `upload` does about the pixel buffer for a frame of a given size.
#[derive(Debug, PartialEq, Eq)]
enum BufferPlan {
    /// The buffer already has the frame's size.
    Present,
    /// Resize the buffer first.
    Resize,
    /// A resize failed earlier: leave the picture alone rather than retry (and
    /// warn) every frame.
    Skip,
}

fn buffer_plan(frame: (u32, u32), buffer: (u32, u32), resize_failed: bool) -> BufferPlan {
    if frame == buffer {
        BufferPlan::Present
    } else if resize_failed {
        BufferPlan::Skip
    } else {
        BufferPlan::Resize
    }
}

/// The window title: the program name, then the game's title if it has one.
/// Both systems use `gb` (the binary's name), so a GBA game is `gb - <title>`.
fn window_title(game: &str) -> String {
    if game.is_empty() {
        "gb".to_owned()
    } else {
        format!("gb - {game}")
    }
}

/// State for `--test-pattern`: the core is bypassed.
struct TestMode {
    frame: u32,
    fb: Box<Framebuffer>,
    tone: ToneGen,
}

/// Builds the core for a ROM file, for the ROM dropped on the window, with the
/// files its player data goes to. The error is a ready-to-print message.
pub type Loader<C> = Box<dyn FnMut(&Path) -> Result<(C, GamePaths), String>>;

/// Where progress is persisted: battery RAM, the resume point, snapshots and screenshots.
pub struct Storage {
    pub saves: SaveManager,
    pub states: StateStore,
    pub paths: GamePaths,
}

/// The picture to show: the test pattern if that mode is on, else the core's.
fn frame_of<'a>(test: Option<&'a TestMode>, core: &'a impl Core) -> FrameRef<'a> {
    match test {
        Some(t) => FrameRef {
            width: SCREEN_WIDTH,
            height: SCREEN_HEIGHT,
            pixels: FramePixels::Shades(&t.fb[..]),
        },
        None => core.frame(),
    }
}

/// Window, input, pacing and presentation for any [`Core`]. `main` uses
/// `App<Box<dyn Core>>` (the system is chosen at run time); tests use a
/// concrete machine to reach its debug hooks.
pub struct App<C: Core> {
    core: C,
    /// The running game's saves, resume point, snapshots and screenshot folder; `None` when no game
    /// is loaded (the library), so nothing can write on a game's behalf then.
    storage: Option<Storage>,
    /// Builds the core that stands in while no game is loaded; `None` keeps the last one.
    idle: Option<Box<dyn Fn() -> Option<C>>>,
    audio: Option<AudioOutput>,
    /// Reused every frame to carry samples from the core to the ring.
    audio_buf: Vec<f32>,
    palette: Palette,
    scale: u32,
    title: String,
    /// Size of the `pixels` buffer; follows the core's frame size.
    buffer_size: (u32, u32),
    /// A buffer resize failed (and was reported once); see [`BufferPlan::Skip`].
    resize_failed: bool,
    /// Snapshot feedback shown in the title until the given instant.
    status: Option<(String, Instant)>,
    test: Option<TestMode>,
    shutdown: ShutdownFlag,
    /// Always shown in the window title, e.g. that there is no data folder.
    notice: Option<String>,
    /// Powers on a dropped ROM; `None` ignores drops.
    loader: Option<Loader<C>>,
    recorder: Option<Recorder>,
    /// Emulated frames since power-on (the clock recordings are keyed by).
    frame: u64,

    input: InputState,
    /// The buttons the core has been told are down. Recordings and releases follow this,
    /// not the keys, so nothing is released or pressed twice.
    core_held: HashSet<Button>,
    /// The frame size the window was last sized for, `None` while it has the library's.
    fitted: Option<(u32, u32)>,
    pacer: FramePacer,
    last_tick: Instant,
    /// The emulator is stopped: any mode but `Playing`. Kept in step by `set_mode`.
    paused: bool,
    mode: Mode,
    fast_forward: bool,
    /// False while the window has a zero-sized surface (minimized).
    surface_ok: bool,

    /// The game list; `None` when there is no data folder to keep one in.
    library: Option<Library>,
    /// The library game running, if it is one (the key its thumbnail is stored by).
    game_id: Option<GameId>,
    /// The ROM file the running game was started from, to power it on again (Restart).
    rom_path: Option<PathBuf>,
    /// Games leave a resume point and start from it. Off for `--record` and
    /// `--test-pattern` (a recording must start from power-on), for the whole run.
    resume_enabled: bool,
    /// When the resume point is next written during play.
    next_resume: Instant,
    /// No resume point is written while this game runs: one exists that this run did not
    /// use (a power-on option was given, or it could not be put aside), and writing would
    /// replace it. Cleared when another game starts.
    freeze_resume: bool,
    /// The library games that have a resume point, read when the library is entered or
    /// changed (the cards are drawn from this, not from the disk).
    resumable: HashSet<GameId>,
    /// The running game's snapshots as the pause menu lists them, newest first. Read when
    /// the menu opens and after a change, not on every frame.
    snapshot_list: Vec<SnapshotItem>,
    thumbnail: ThumbnailTrigger,
    toasts: Toasts,
    /// When egui asked to be drawn again by itself (an animation).
    ui_wake: Option<Instant>,
    gui: Option<Gui>,

    window: Option<Arc<Window>>,
    pixels: Option<Pixels<'static>>,
    /// Set when the app must stop with a failure; reported by `main`.
    pub error: Option<String>,
}

impl<C: Core> App<C> {
    pub fn new(
        core: C,
        storage: Storage,
        audio: Option<AudioOutput>,
        palette: Palette,
        scale: u32,
        test_pattern: bool,
        shutdown: ShutdownFlag,
    ) -> Self {
        let title = window_title(&core.title());
        let fps = core.native_fps();
        let test = test_pattern.then(|| TestMode {
            frame: 0,
            fb: Box::new([0; SCREEN_WIDTH * SCREEN_HEIGHT]),
            tone: ToneGen::new(
                audio
                    .as_ref()
                    .map_or(core.audio_sample_rate(), |a| a.sample_rate),
                fps,
            ),
        });
        Self {
            core,
            storage: Some(storage),
            idle: None,
            audio,
            audio_buf: Vec::new(),
            palette,
            scale,
            title,
            buffer_size: (SCREEN_WIDTH as u32, SCREEN_HEIGHT as u32),
            resize_failed: false,
            status: None,
            test,
            shutdown,
            notice: None,
            loader: None,
            recorder: None,
            frame: 0,
            input: InputState::default(),
            core_held: HashSet::new(),
            fitted: None,
            pacer: FramePacer::new(fps),
            last_tick: Instant::now(),
            paused: false,
            mode: Mode::Playing,
            fast_forward: false,
            surface_ok: true,
            library: None,
            game_id: None,
            rom_path: None,
            resume_enabled: false,
            next_resume: Instant::now() + RESUME_INTERVAL,
            freeze_resume: false,
            resumable: HashSet::new(),
            snapshot_list: Vec::new(),
            thumbnail: ThumbnailTrigger::default(),
            toasts: Toasts::default(),
            ui_wake: None,
            gui: None,
            window: None,
            pixels: None,
            error: None,
        }
    }

    /// Show `notice` in the window title for the whole session.
    pub fn with_notice(mut self, notice: Option<String>) -> Self {
        self.notice = notice;
        self
    }

    /// Power on a ROM dropped onto the window with `loader`.
    pub fn with_loader(mut self, loader: Loader<C>) -> Self {
        self.loader = Some(loader);
        self
    }

    /// Record every input from power-on (see `record.rs`).
    pub fn with_recorder(mut self, recorder: Option<Recorder>) -> Self {
        // A recording must start from power-on to replay, so there is no resume.
        if recorder.is_some() {
            self.resume_enabled = false;
        }
        self.recorder = recorder;
        self
    }

    /// Let games leave a resume point when they are left and start from it (see
    /// `resume_enabled`). Does nothing for the test pattern.
    pub fn with_resume(mut self, enabled: bool) -> Self {
        self.resume_enabled = enabled && self.recorder.is_none() && self.test.is_none();
        self
    }

    /// The ROM file the running game was started from, so Restart can power it on again.
    pub fn with_rom_path(mut self, path: Option<PathBuf>) -> Self {
        self.rom_path = path;
        self
    }

    /// Continue the game given to [`App::new`] from its resume point, if resume is on and
    /// it has one (a game started from the command line; a library game is resumed as it is
    /// opened). The battery save is already loaded.
    ///
    /// `forced` are the power-on options the player passed on the command line (`--model`,
    /// `--rtc-start`, ...). The resume point was made with other ones, so it is not used;
    /// the game starts fresh and the player is told. The resume point is left untouched:
    /// nothing is written to it while this game runs (the battery save and snapshots are
    /// unaffected), so a plain start afterwards still continues from it.
    pub fn continue_game(mut self, forced: &[&'static str]) -> Self {
        if !forced.is_empty() {
            let has_point = self.storage.as_ref().is_some_and(|s| s.states.has_resume());
            if self.resume_enabled && self.test.is_none() && has_point {
                self.freeze_resume = true;
                let text = format!(
                    "Started fresh because of {}; your resume point is kept (open from the \
                     library to continue)",
                    forced.join(", ")
                );
                log::info!("{text}");
                self.toast_long(log::Level::Info, text);
            }
            return self;
        }
        if self.try_resume() {
            for button in Button::ALL {
                self.set_button(button, self.input.is_down(button));
            }
        }
        self
    }

    /// Keep the game list. `None`: there is no data folder to keep one in, and the
    /// library screen only offers to play a ROM.
    pub fn with_library(mut self, library: Option<Library>) -> Self {
        self.library = library;
        self
    }

    /// The core that stands in while no game is loaded (the library).
    pub fn with_idle(mut self, idle: Box<dyn Fn() -> Option<C>>) -> Self {
        self.idle = Some(idle);
        self
    }

    /// Start on the game list; the core given to [`App::new`] is a placeholder that
    /// is never stepped, and its storage is dropped unused.
    pub fn in_library(mut self) -> Self {
        self.detach_game();
        self.set_mode(Mode::Library);
        self.refresh_resumable();
        self.title = window_title("");
        self
    }

    /// Let go of the running game: its saves, states and screenshot folder, and, if
    /// there is a stand-in, its core. Nothing can act on the game afterwards.
    fn detach_game(&mut self) {
        self.storage = None;
        self.rom_path = None;
        self.freeze_resume = false;
        self.snapshot_list.clear();
        if let Some(core) = self.idle.as_ref().and_then(|idle| idle()) {
            self.core = core;
        }
        self.game_id = None;
        self.thumbnail = ThumbnailTrigger::default();
        self.core_held.clear();
        self.frame = 0;
    }

    /// The running game's files, if a game is loaded.
    #[cfg(test)]
    fn game_paths(&self) -> Option<&GamePaths> {
        self.storage.as_ref().map(|s| &s.paths)
    }

    /// The running game is the library game `id` (if it is listed): it is marked as
    /// played, and gets a thumbnail once it has run a while if it has none.
    pub fn with_game(mut self, id: Option<GameId>) -> Self {
        self.set_game(id);
        self
    }

    fn set_game(&mut self, id: Option<GameId>) {
        // `Some(wanted)` for a listed game: wanted when it has no thumbnail yet.
        let listed = match (&id, self.library.as_ref()) {
            (Some(id), Some(library)) => library.get(id).map(|e| e.thumbnail.is_none()),
            _ => None,
        };
        self.thumbnail = ThumbnailTrigger::new(listed == Some(true));
        self.game_id = listed.and(id);
        if let (Some(id), Some(library)) = (&self.game_id, self.library.as_mut())
            && let Err(e) = library.touch_played(id)
        {
            log::warn!("{e}");
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.paused = !mode.runs_emulation();
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Show `text` in a toast for a few seconds.
    fn toast(&mut self, level: log::Level, text: impl Into<String>) {
        self.toasts.push(level, text, Instant::now());
        self.request_redraw();
    }

    /// Show `text` in a toast long enough to read a sentence or two ([`LONG_TOAST_DURATION`]).
    fn toast_long(&mut self, level: log::Level, text: impl Into<String>) {
        self.toasts
            .push_for(level, text, Instant::now(), LONG_TOAST_DURATION);
        self.request_redraw();
    }

    /// Read which library games have a resume point (see `resumable`).
    fn refresh_resumable(&mut self) {
        self.resumable = self.library.as_ref().map_or_else(HashSet::new, |library| {
            library
                .entries()
                .iter()
                .filter(|e| library.has_resume(&e.id))
                .map(|e| e.id.clone())
                .collect()
        });
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, msg: String) {
        self.error = Some(msg);
        event_loop.exit();
    }

    /// Exit the loop (and so flush saves) if a termination signal was received.
    fn check_shutdown(&self, event_loop: &ActiveEventLoop) -> bool {
        let requested = self.shutdown.requested();
        if requested && !event_loop.exiting() {
            log::info!("received a termination signal; saving and exiting");
            event_loop.exit();
        }
        requested
    }

    fn reset_clock(&mut self) {
        self.pacer.reset();
        self.last_tick = Instant::now();
    }

    fn clear_audio(&self) {
        if let Some(a) = &self.audio {
            a.ring.clear();
        }
    }

    fn update_title(&self) {
        if let Some(w) = &self.window {
            let rec = if self.recorder.is_some() {
                " [rec]"
            } else {
                ""
            };
            let paused = if self.paused && self.mode != Mode::Library {
                " [paused]"
            } else {
                ""
            };
            let notice = self
                .notice
                .as_ref()
                .map_or(String::new(), |n| format!(" [{n}]"));
            let status = self
                .status
                .as_ref()
                .map_or(String::new(), |(msg, _)| format!(" - {msg}"));
            w.set_title(&format!("{}{notice}{rec}{paused}{status}", self.title));
        }
    }

    /// Report a result in the log, at `level` so failures show up as warnings and
    /// errors, and, briefly, in the title (an error with its `error: ` prefix). Problems
    /// also get a toast, and so does everything while a menu is up: its buttons have no
    /// other way to answer.
    fn show_status(&mut self, level: log::Level, msg: String) {
        log::log!(level, "{msg}");
        if level <= log::Level::Warn || self.mode.menu_visible() {
            self.toast(level, msg.clone());
        }
        let shown = if level == log::Level::Error {
            format!("error: {msg}")
        } else {
            msg
        };
        self.status = Some((shown, Instant::now() + STATUS_DURATION));
        self.update_title();
    }

    /// Drop an expired status message; returns when the current one expires.
    fn expire_status(&mut self, now: Instant) -> Option<Instant> {
        match &self.status {
            Some((_, until)) if now >= *until => {
                self.status = None;
                self.update_title();
                None
            }
            Some((_, until)) => Some(*until),
            None => None,
        }
    }

    /// End the recording now (writing its `End` record) and say why.
    fn stop_recording(&mut self, why: &str) {
        if let Some(mut r) = self.recorder.take() {
            log::info!("recording stopped: {why}, so the rest could not be replayed");
            r.finish(self.frame);
            self.update_title();
        }
    }

    /// Say `text` in the title and in a toast, at `level`.
    fn announce(&mut self, level: log::Level, text: String) {
        self.show_status(level, text.clone());
        self.toast(level, text);
    }

    /// Write the resume point of the running game (after its battery save, so the save is
    /// never older than the point). Nothing when resume is off, as under `--record`.
    fn write_resume(&mut self) {
        if !self.resume_enabled
            || self.freeze_resume
            || self.test.is_some()
            || !self.core.supports_save_state()
        {
            return;
        }
        let Some(storage) = self.storage.as_ref() else {
            return;
        };
        if let Err(e) = storage.states.save_resume(&self.core) {
            log::warn!("cannot save the resume point: {e}");
        }
    }

    /// Continue the running game from its resume point. Returns whether it did. A resume
    /// point that cannot be used is put aside (never deleted, never overwritten), the game
    /// starts normally and the player is told, in a toast that stays long enough to read.
    /// If the file could not be put aside it is still there, so no resume point is written
    /// for this game (see `freeze_resume`).
    fn try_resume(&mut self) -> bool {
        if !self.resume_enabled || self.test.is_some() {
            return false;
        }
        let Some(storage) = self.storage.as_mut() else {
            return false;
        };
        match storage.states.load_resume(&mut self.core) {
            Resume::Absent => false,
            Resume::Loaded { backup } => {
                if let Some(backup) = backup {
                    log::info!(
                        "the game was saved after the resume point; its save is kept as {}",
                        backup.display()
                    );
                }
                log::info!("resumed {}", self.core.title());
                self.toast(log::Level::Info, "Resumed where you left off");
                true
            }
            Resume::Failed { why, aside, kept } => {
                if kept.is_none() && storage.states.has_resume() {
                    self.freeze_resume = true;
                }
                let name = kept.as_ref().map(|k| {
                    k.file_name().map_or_else(
                        || k.display().to_string(),
                        |n| n.to_string_lossy().into_owned(),
                    )
                });
                let text = match (aside, name) {
                    (Aside::Mismatch, Some(name)) => format!(
                        "Could not resume ({why}). Started from the beginning. Your resume \
                         point is kept as {name} in the game's states folder: start with the \
                         original options and rename it to resume.state to continue."
                    ),
                    (Aside::Bad, Some(name)) => format!(
                        "Could not resume ({why}). Started from the beginning. The old file \
                         was kept as {name}."
                    ),
                    (_, None) => format!(
                        "Could not resume ({why}). Started from the beginning. The old resume \
                         point is left where it is and not overwritten."
                    ),
                };
                self.show_status(log::Level::Warn, text.clone());
                self.toast_long(log::Level::Warn, text);
                false
            }
        }
    }

    /// Read the snapshots of the running game for the pause menu.
    fn refresh_snapshots(&mut self) {
        self.snapshot_list = self.storage.as_ref().map_or_else(Vec::new, |s| {
            snapshot_items(s.states.before_load(), s.states.snapshots())
        });
    }

    /// Add a snapshot of the running game: its state, the picture on screen and the time.
    fn save_snapshot(&mut self) {
        let Some(storage) = self.storage.as_ref() else {
            return;
        };
        let picture = record::encode_png(frame_of(self.test.as_ref(), &self.core));
        if let Err(e) = &picture {
            log::warn!("the snapshot gets no picture: {e}");
        }
        let saved = storage.states.save_snapshot(
            &self.core,
            picture.as_deref().ok(),
            crate::library::now_secs(),
        );
        let before_load = storage.states.before_load();
        match saved {
            Ok(saved) => {
                self.snapshot_list = snapshot_items(before_load, saved.snapshots);
                let text = if saved.evicted > 0 {
                    format!(
                        "Snapshot saved. The oldest one was removed; at most {} are kept.",
                        crate::state::MAX_SNAPSHOTS
                    )
                } else {
                    "Snapshot saved".to_owned()
                };
                self.announce(log::Level::Info, text);
            }
            Err(msg) => self.show_status(log::Level::Warn, msg),
        }
    }

    /// L: load the most recent snapshot.
    fn load_latest_snapshot(&mut self) {
        let Some(storage) = self.storage.as_ref() else {
            return;
        };
        match storage.states.snapshots().first() {
            Some(latest) => {
                let secs = latest.secs;
                self.load_snapshot(secs);
            }
            None => self.announce(log::Level::Info, "No snapshots yet".to_owned()),
        }
    }

    /// Load the snapshot taken at `secs` (see [`Self::load_slot`]).
    fn load_snapshot(&mut self, secs: u64) -> bool {
        self.load_slot(Slot::Snapshot(secs))
    }

    /// Load `slot`, keeping where the game was (with the picture on screen) as "Before last
    /// load". Returns whether it loaded; if not, the machine is as it was and the reason is
    /// shown.
    fn load_slot(&mut self, slot: Slot) -> bool {
        if self.storage.is_none() {
            return false;
        }
        let picture = record::encode_png(frame_of(self.test.as_ref(), &self.core)).ok();
        let Some(storage) = self.storage.as_mut() else {
            return false;
        };
        let result = storage
            .states
            .load(&mut self.core, slot, picture.as_deref());
        // A snapshot that was gone or unreadable should not stay in the list.
        self.refresh_snapshots();
        match result {
            Ok(backup) => {
                // The state replaces the machine (and the frame it is on), so
                // the replay of a recording that goes on would diverge.
                self.stop_recording("a snapshot was loaded");
                // The state holds the buttons pressed when it was saved; apply
                // the keys held now instead.
                for button in Button::ALL {
                    self.set_button(button, self.input.is_down(button));
                }
                self.clear_audio();
                self.reset_clock();
                // Show the restored picture even while paused.
                self.upload();
                self.request_redraw();
                if let Some(backup) = backup {
                    log::info!("the previous save is kept as {}", backup.display());
                }
                let text = match slot {
                    Slot::Snapshot(secs) => format!(
                        "Loaded snapshot from {} \u{2014} your previous state is kept as \
                         'Before last load'",
                        friendly_time_local(secs)
                    ),
                    Slot::BeforeLoad => "Back to before the last load \u{2014} the state you \
                                         left is kept as 'Before last load'"
                        .to_owned(),
                };
                self.announce(log::Level::Info, text);
                true
            }
            Err(msg) => {
                self.show_status(log::Level::Warn, msg);
                false
            }
        }
    }

    fn delete_snapshot(&mut self, secs: u64) {
        let Some(storage) = self.storage.as_ref() else {
            return;
        };
        storage.states.delete_snapshot(secs);
        if let Some(gui) = self.gui.as_mut() {
            gui.forget_snapshot(secs);
        }
        self.refresh_snapshots();
        self.announce(log::Level::Info, "Snapshot deleted".to_owned());
    }

    /// Power-cycle the running game: it is read from its ROM again and starts from the
    /// beginning, keeping its battery save. The resume point is dropped with it.
    fn restart_game(&mut self) {
        let Some(path) = self.rom_path.clone() else {
            return;
        };
        let id = self.game_id.clone();
        if self.open_rom(&path, id, true) {
            self.toast(log::Level::Info, "Game restarted");
        }
    }

    /// A ROM dropped onto the window: play it, in place of the running game, and add it
    /// to the library, if there is one, once it has powered on. On failure the running
    /// game carries on, the library is untouched and the error is shown.
    fn open_dropped(&mut self, path: PathBuf) {
        // `--test-pattern` has no game to replace, and without a loader nothing can play.
        if self.test.is_some() || self.loader.is_none() {
            return;
        }
        if !self.open_rom(&path, None, false) {
            return;
        }
        let Some(library) = self.library.as_mut() else {
            return;
        };
        match library.add_rom(&path) {
            Ok(added) => {
                let name = &added.entry.title;
                let msg = if added.duplicate {
                    format!("{name} is already in the library")
                } else {
                    format!("Added {name}")
                };
                self.set_game(Some(added.entry.id.clone()));
                self.toast(log::Level::Info, msg);
            }
            Err(msg) => self.show_status(
                log::Level::Warn,
                format!("playing, but not added to the library: {msg}"),
            ),
        }
    }

    /// Power on the ROM at `path` in place of the running game, as library game `id`
    /// if it is one. Returns whether it started; if not, the running game carries on
    /// and the error is shown. The running game is left with a resume point and the new
    /// one continues from its own, unless `fresh` (Start fresh, Restart): then the old
    /// game's point is not written and the new game's is discarded.
    fn open_rom(&mut self, path: &Path, id: Option<GameId>, fresh: bool) -> bool {
        let Some(loader) = self.loader.as_mut() else {
            return false;
        };
        let (mut core, paths) = match loader(path) {
            Ok(loaded) => loaded,
            Err(msg) => {
                self.show_status(log::Level::Error, msg);
                return false;
            }
        };
        // The old game's progress is written before anything is replaced.
        if let Some(storage) = self.storage.as_mut() {
            storage.saves.flush_final(&self.core);
        }
        if !fresh {
            self.write_resume();
        }
        self.stop_recording("another ROM was opened");
        let rate_check = self.audio.as_ref().map(|a| {
            core.set_audio_sample_rate(a.sample_rate);
            audio::check_core_rate(a.sample_rate, core.audio_sample_rate())
        });
        if let Some(Err(e)) = rate_check {
            log::warn!("audio disabled: {e}");
            self.audio = None;
        }
        let mut saves = SaveManager::new(&paths.sav, Instant::now());
        saves.load(&mut core);

        self.core = core;
        self.storage = Some(Storage {
            saves,
            states: StateStore::new(paths.clone()),
            paths,
        });
        self.rom_path = Some(path.to_owned());
        self.freeze_resume = false;
        self.refresh_snapshots();
        self.next_resume = Instant::now() + RESUME_INTERVAL;
        if fresh {
            if self.resume_enabled
                && let Some(storage) = self.storage.as_ref()
            {
                storage.states.discard_resume();
            }
        } else {
            self.try_resume();
        }
        self.title = window_title(&self.core.title());
        self.pacer = FramePacer::new(self.core.native_fps());
        if self.fast_forward {
            self.pacer.set_speed(FAST_FORWARD_SPEED);
        }
        self.frame = 0;
        self.set_mode(self.mode.play());
        self.ui_wake = None;
        self.reset_ui();
        self.set_game(id);
        // The new machine starts with no buttons down; apply the keys held now.
        self.core_held.clear();
        for button in Button::ALL {
            self.set_button(button, self.input.is_down(button));
        }
        self.clear_audio();
        self.reset_clock();
        self.update_title();
        self.upload();
        self.fit_window();
        self.request_redraw();
        log::info!("opened {} ({})", path.display(), self.core.title());
        true
    }

    /// The logical window size to ask for now, or `None` to leave the window as it is:
    /// it is sized for a game when the picture changes size or when it has had the
    /// library's size, and otherwise left alone, as the player may have resized it.
    fn fit_request(&mut self) -> Option<(u32, u32)> {
        let frame = frame_of(self.test.as_ref(), &self.core);
        let size = (frame.width as u32, frame.height as u32);
        if self.fitted == Some(size) {
            return None;
        }
        self.fitted = Some(size);
        Some((size.0 * self.scale, size.1 * self.scale))
    }

    fn fit_window(&mut self) {
        let Some((width, height)) = self.fit_request() else {
            return;
        };
        if let Some(w) = &self.window {
            // The window manager may answer with another size; that is its right.
            let _ = w.request_inner_size(LogicalSize::new(width, height));
        }
    }

    fn reset_ui(&mut self) {
        if let Some(gui) = self.gui.as_mut() {
            gui.reset_view();
        }
    }

    /// P toggles the pause menu, like Esc does.
    fn toggle_pause(&mut self) {
        match self.mode {
            Mode::Playing => self.pause(),
            Mode::Paused => self.resume(),
            Mode::Library => {}
        }
    }

    /// Stop the game and show the pause menu.
    fn pause(&mut self) {
        if self.mode != Mode::Playing {
            return;
        }
        self.set_mode(self.mode.pause());
        // The periodic save tick doesn't run while paused; persist now.
        if let Some(storage) = self.storage.as_mut() {
            storage.saves.flush(&self.core);
        }
        self.refresh_snapshots();
        // Keys keep being tracked in the menu but no longer reach the game, so let go
        // of what the game holds now; `resume` presses again what still is held.
        self.release_core_buttons();
        self.ui_wake = None;
        self.clear_audio();
        self.reset_clock();
        self.update_title();
        self.request_redraw();
    }

    /// Close the pause menu and carry on.
    fn resume(&mut self) {
        if self.mode != Mode::Paused {
            return;
        }
        self.set_mode(self.mode.resume());
        self.ui_wake = None;
        self.reset_ui();
        for button in Button::ALL {
            if self.input.is_down(button) && !self.core_held.contains(&button) {
                self.set_button(button, true);
            }
        }
        self.clear_audio();
        self.reset_clock();
        self.update_title();
        self.request_redraw();
    }

    /// Release every button the game is holding, and only those.
    fn release_core_buttons(&mut self) {
        for button in Button::ALL {
            if self.core_held.contains(&button) {
                self.set_button(button, false);
            }
        }
    }

    /// Leave the game for the library: its progress is written as when quitting, then
    /// the game is let go of, so no later tick, flush or screenshot reaches it.
    fn back_to_library(&mut self) {
        if let Some(storage) = self.storage.as_mut() {
            storage.saves.flush_final(&self.core);
        }
        self.write_resume();
        self.stop_recording("returned to the library");
        self.release_core_buttons();
        self.detach_game();
        self.refresh_resumable();
        self.set_mode(self.mode.back_to_library());
        self.title = window_title("");
        self.set_fast_forward(false);
        self.ui_wake = None;
        self.reset_ui();
        self.clear_audio();
        self.reset_clock();
        self.update_title();
        self.request_redraw();
    }

    /// Whether Esc may open the pause menu: not for `--test-pattern`, which has no game
    /// to pause, where Esc quits as it always did.
    fn can_pause(&self) -> bool {
        self.test.is_none()
    }

    /// What Esc does now.
    fn escape(&mut self) -> Effect {
        let overlay = self.gui.as_ref().is_some_and(Gui::overlay_open);
        match escape_effect(self.mode, overlay, self.can_pause()) {
            EscapeEffect::Quit => return Effect::Exit,
            EscapeEffect::Pause => self.pause(),
            EscapeEffect::Resume => self.resume(),
            EscapeEffect::CloseOverlay => {
                if let Some(gui) = self.gui.as_mut() {
                    gui.close_overlay();
                }
                self.request_redraw();
            }
        }
        Effect::None
    }

    fn set_fast_forward(&mut self, on: bool) {
        if self.fast_forward != on {
            self.fast_forward = on;
            self.pacer
                .set_speed(if on { FAST_FORWARD_SPEED } else { 1.0 });
            self.clear_audio();
            self.reset_clock();
        }
    }

    /// Every joypad change goes through here so recordings see all of them.
    /// A button the console lacks (L/R on a Game Boy) is dropped here, so it
    /// never reaches a recording the system's decoder would then reject.
    fn set_button(&mut self, button: Button, pressed: bool) {
        if !self.core.system().has_button(button) {
            return;
        }
        self.core.set_button(button, pressed);
        if pressed {
            self.core_held.insert(button);
        } else {
            self.core_held.remove(&button);
        }
        if let Some(r) = self.recorder.as_mut() {
            r.record(Record {
                frame: self.frame,
                event: Event::Button { button, pressed },
            });
        }
    }

    /// A key event from the window. A repeat (the key held down) does nothing: every key
    /// action happens once per press. Whether it is one is winit's word, not the held-key
    /// state, so a key whose release never came (macOS swallows it under Cmd) still acts on
    /// its next press.
    fn on_key_event(&mut self, key: winit::keyboard::KeyCode, down: bool, repeat: bool) -> Effect {
        if repeat {
            return Effect::None;
        }
        self.on_key(key, down)
    }

    /// A key went down or up (not a repeat). Keys are always tracked, so one still held
    /// when a menu closes is pressed in the game; but only while the game is running do
    /// they reach it. Esc belongs to the app in every mode.
    fn on_key(&mut self, key: winit::keyboard::KeyCode, down: bool) -> Effect {
        self.on_key_input(key, down);
        match map(key) {
            Some(Action::Menu) if down => return self.escape(),
            Some(Action::Pause) if down => self.toggle_pause(),
            // A Tab released over a menu must still end fast-forward.
            Some(Action::FastForward) => self.set_fast_forward(down && self.mode.game_input()),
            // Snapshots need the core, which `--test-pattern` bypasses.
            _ if !down || self.test.is_some() || !self.mode.game_input() => {}
            Some(Action::SaveSnapshot) => self.save_snapshot(),
            Some(Action::LoadLatestSnapshot) => self.load_latest_snapshot(),
            _ => {}
        }
        Effect::None
    }

    /// The part of key handling that needs no event loop: the joypad and the screenshot
    /// key, which only act while the game is running.
    fn on_key_input(&mut self, key: winit::keyboard::KeyCode, down: bool) {
        let running = self.mode.game_input();
        if let Some((button, pressed)) = self.input.key(key, down)
            && running
        {
            self.set_button(button, pressed);
        }
        if down && running && map(key) == Some(Action::Screenshot) {
            self.screenshot();
        }
    }

    fn release_all_buttons(&mut self) {
        for button in self.input.release_all() {
            if self.core_held.contains(&button) {
                self.set_button(button, false);
            }
        }
    }

    /// Save the picture on screen in the game's screenshots and mark the frame.
    fn screenshot(&mut self) {
        let Some(storage) = self.storage.as_ref() else {
            return;
        };
        let path = storage
            .paths
            .screenshot(crate::library::now_secs(), self.frame);
        let frame = frame_of(self.test.as_ref(), &self.core);
        match record::write_png(frame, &path) {
            Ok(()) => {
                log::info!("screenshot: {} (frame {})", path.display(), self.frame);
                if self.mode.menu_visible() {
                    self.toast(log::Level::Info, "Screenshot saved");
                }
            }
            Err(e) => self.show_status(
                log::Level::Warn,
                format!("cannot write screenshot {}: {e}", path.display()),
            ),
        }
        if let Some(r) = self.recorder.as_mut() {
            r.record(Record {
                frame: self.frame,
                event: Event::Marker,
            });
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        let extent = surface_extent(width, height);
        self.surface_ok = extent.is_some();
        let Some((width, height)) = extent else {
            return; // minimised: keep the old surface and skip drawing until restored
        };
        if let Some(p) = self.pixels.as_mut()
            && let Err(e) = p.resize_surface(width, height)
        {
            log::warn!("cannot resize surface to {width}x{height}: {e}");
        }
        // Restored from minimised while paused: nothing else would repaint.
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Run one `step_frame`, feed audio, and return the emulated seconds it
    /// covered (see [`audio_seconds`]): usually one frame, but a CGB speed
    /// switch makes a step span 0.4-1.9 frames.
    fn step_once(&mut self) -> f64 {
        self.audio_buf.clear();
        let emulated = match self.test.as_mut() {
            Some(t) => {
                test_pattern(t.frame, &mut t.fb);
                t.frame = t.frame.wrapping_add(1);
                self.audio_buf.extend_from_slice(&t.tone.next_frame());
                self.pacer.period()
            }
            None => {
                self.core.step_frame();
                self.frame += 1;
                // Only at normal speed: fast-forward would shorten the ten seconds.
                if !self.fast_forward && self.thumbnail.frame() {
                    self.try_thumbnail();
                }
                // Always drain so the core's buffer cannot grow unbounded.
                self.core.drain_audio_into(&mut self.audio_buf);
                let seconds = audio_seconds(self.audio_buf.len(), self.core.audio_sample_rate());
                // A step that drained no samples still covered a frame: without
                // this the pacer would see zero time pass and run flat out. Both
                // shipped cores always emit samples, so this only guards a core
                // that stops doing so.
                if seconds > 0.0 {
                    seconds
                } else {
                    self.pacer.period()
                }
            }
        };
        if !self.fast_forward
            && let Some(a) = &self.audio
        {
            a.ring.push(&self.audio_buf);
        }
        emulated
    }

    /// Run steps while the pacer says a frame is owed ([`run_due`]), then
    /// upload the last picture. Returns whether any step ran.
    fn run_due(&mut self) -> bool {
        let ran = run_due(self);
        if ran > 0 {
            self.upload();
        }
        ran > 0
    }

    /// Take the thumbnail now, unless the screen is a single colour (a fade, a loading
    /// screen): then the trigger asks again a little later, up to its limit.
    fn try_thumbnail(&mut self) {
        if frame_is_uniform(&self.core.frame()) {
            return;
        }
        self.thumbnail.done();
        self.save_thumbnail();
    }

    /// Make the picture on screen the library thumbnail of the running game.
    fn save_thumbnail(&mut self) {
        let (Some(id), Some(library)) = (self.game_id.clone(), self.library.as_mut()) else {
            return;
        };
        let png = match record::encode_png(self.core.frame()) {
            Ok(png) => png,
            Err(e) => return log::warn!("cannot make a thumbnail of {id}: {e}"),
        };
        match library.set_thumbnail(&id, &png) {
            Ok(path) => {
                log::info!("thumbnail of {id}: {}", path.display());
                if let Some(gui) = self.gui.as_mut() {
                    gui.forget_thumbnail(&id);
                }
            }
            Err(e) => log::warn!("cannot save the thumbnail of {id}: {e}"),
        }
    }

    fn upload(&mut self) {
        let frame = frame_of(self.test.as_ref(), &self.core);
        let size = (frame.width as u32, frame.height as u32);
        let Some(p) = self.pixels.as_mut() else {
            return;
        };
        match buffer_plan(size, self.buffer_size, self.resize_failed) {
            BufferPlan::Present => {}
            BufferPlan::Skip => return,
            BufferPlan::Resize => match p.resize_buffer(size.0, size.1) {
                Ok(()) => self.buffer_size = size,
                Err(e) => {
                    self.resize_failed = true;
                    log::warn!(
                        "cannot resize the frame buffer to {}x{}: {e}; the picture stops updating",
                        size.0,
                        size.1
                    );
                    return;
                }
            },
        }
        present(&frame, &self.palette, p.frame_mut());
    }
}

impl<C: Core> Steps for App<C> {
    fn pacer(&mut self) -> &mut FramePacer {
        &mut self.pacer
    }

    fn step(&mut self) -> f64 {
        self.step_once()
    }
}

impl<C: Core> App<C> {
    /// The periodic battery save check, and the resume point every [`RESUME_INTERVAL`];
    /// nothing without a game.
    fn tick_saves(&mut self, now: Instant) {
        if let Some(storage) = self.storage.as_mut() {
            storage.saves.tick(&self.core, now);
        }
        if now >= self.next_resume {
            self.next_resume = now + RESUME_INTERVAL;
            self.write_resume();
        }
    }

    /// The last writes, as the program exits.
    fn finish(&mut self) {
        if let Some(storage) = self.storage.as_mut() {
            storage.saves.flush_final(&self.core);
        }
        self.write_resume();
        if let Some(r) = self.recorder.as_mut() {
            r.finish(self.frame);
        }
    }

    /// Draw the window: just the game's picture while playing, as before, or the
    /// screens (and toasts) over or instead of it.
    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        if !self.surface_ok {
            return;
        }
        self.toasts.expire(Instant::now());
        let show_ui = self.mode.menu_visible() || !self.toasts.is_empty();
        let (Some(window), Some(pixels)) = (self.window.as_ref(), self.pixels.as_ref()) else {
            return;
        };
        let can_save = self.storage.is_some() && self.test.is_none();
        let can_restart = can_save && self.loader.is_some() && self.rom_path.is_some();
        let can_return = self.library.is_some() && self.test.is_none();
        let gui = match self.gui.as_mut() {
            Some(gui) if show_ui => gui,
            _ => {
                if let Err(e) = pixels.render() {
                    self.fail(event_loop, format!("render failed: {e}"));
                }
                return;
            }
        };
        let game_title = self.core.title();
        let screen = Screen {
            mode: self.mode,
            library: self.library.as_ref(),
            data_dir: self.library.as_ref().map(|l| l.data_dir().root()),
            game_title: &game_title,
            can_save,
            can_restart,
            snapshots: &self.snapshot_list,
            resumable: &self.resumable,
            can_return,
            toasts: &self.toasts,
        };
        let drawn = match gui.draw(window, pixels, &screen) {
            Ok(drawn) => drawn,
            Err(e) => return self.fail(event_loop, format!("render failed: {e}")),
        };
        self.ui_wake = drawn
            .repaint_after
            .filter(|d| !d.is_zero())
            .map(|d| Instant::now() + d);
        if drawn.repaint_after.is_some_and(|d| d.is_zero()) {
            self.request_redraw();
        }
        for action in drawn.actions {
            if self.perform(action) == Effect::Exit {
                event_loop.exit();
            }
        }
    }

    /// Carry out what the player asked for on a screen.
    fn perform(&mut self, action: UiAction) -> Effect {
        match action {
            UiAction::Play(id) => self.play_library_game(&id, false),
            UiAction::StartFresh(id) => self.play_library_game(&id, true),
            UiAction::AddGames => self.add_games(),
            UiAction::Remove { id, delete_saves } => self.remove_game(&id, delete_saves),
            UiAction::OpenDataFolder => self.open_data_folder(),
            UiAction::Resume => self.resume(),
            UiAction::SaveSnapshot if self.test.is_none() => self.save_snapshot(),
            UiAction::LoadSnapshot(secs) if self.test.is_none() => {
                // Back to the game at once, on the picture just loaded.
                if self.load_snapshot(secs) {
                    self.resume();
                }
            }
            UiAction::LoadBeforeLoad if self.test.is_none() => {
                if self.load_slot(Slot::BeforeLoad) {
                    self.resume();
                }
            }
            UiAction::DeleteSnapshot(secs) if self.test.is_none() => self.delete_snapshot(secs),
            UiAction::SaveSnapshot
            | UiAction::LoadSnapshot(_)
            | UiAction::LoadBeforeLoad
            | UiAction::DeleteSnapshot(_) => {}
            UiAction::Restart if self.test.is_none() => self.restart_game(),
            UiAction::Restart => {}
            UiAction::Screenshot => self.screenshot(),
            UiAction::BackToLibrary => self.back_to_library(),
            UiAction::Quit => return Effect::Exit,
        }
        // What was done may have changed the screen.
        self.request_redraw();
        Effect::None
    }

    /// Open library game `id`; `fresh` drops its resume point and powers it on normally.
    fn play_library_game(&mut self, id: &GameId, fresh: bool) {
        let Some(entry) = self.library.as_ref().and_then(|l| l.get(id)) else {
            return;
        };
        if !entry.available {
            let msg = format!(
                "{} is missing. Add its ROM again to bring it back.",
                entry.title
            );
            return self.toast(log::Level::Warn, msg);
        }
        let path = entry.rom_path.clone();
        self.open_rom(&path, Some(id.clone()), fresh);
    }

    /// The Add game button: choose ROMs and add them to the library (or, with no
    /// library, play the first).
    fn add_games(&mut self) {
        let paths = crate::picker::pick_roms();
        // A dialog can stay open for minutes; do not run them over as catch-up.
        self.reset_clock();
        if self.library.is_none() {
            if let Some(first) = paths.into_iter().next() {
                self.open_rom(&first, None, false);
            }
            return;
        }
        for path in paths {
            let Some(library) = self.library.as_mut() else {
                return;
            };
            match library.add_rom(&path) {
                Ok(added) if added.duplicate => self.toast(
                    log::Level::Info,
                    format!("{} is already in the library", added.entry.title),
                ),
                Ok(added) => self.toast(log::Level::Info, format!("Added {}", added.entry.title)),
                Err(msg) => self.show_status(log::Level::Error, msg),
            }
        }
    }

    fn remove_game(&mut self, id: &GameId, delete_saves: bool) {
        let Some(library) = self.library.as_mut() else {
            return;
        };
        let saves = if delete_saves {
            RemoveSaves::Delete
        } else {
            RemoveSaves::Keep
        };
        match library.remove(id, saves) {
            Ok(entry) => {
                self.refresh_resumable();
                if let Some(gui) = self.gui.as_mut() {
                    gui.forget_thumbnail(id);
                }
                self.toast(
                    log::Level::Info,
                    format!("Removed {} from the library", entry.title),
                );
            }
            Err(msg) => self.show_status(log::Level::Error, msg),
        }
    }

    fn open_data_folder(&mut self) {
        let Some(root) = self
            .library
            .as_ref()
            .map(|l| l.data_dir().root().to_owned())
        else {
            return;
        };
        if let Err(e) = opener::open(&root) {
            self.show_status(
                log::Level::Error,
                format!("cannot open {}: {e}", root.display()),
            );
        }
    }

    /// Hand a window event to egui when a screen (or its pointer tracking) needs it.
    /// Returns whether egui wants a redraw.
    fn forward_to_egui(&mut self, event: &WindowEvent) -> bool {
        let wanted = match event {
            // Esc is the app's; every other key reaches egui only over a menu, so
            // egui never sees a game key (nor a held Enter or Space repeating the click
            // of a focused button).
            WindowEvent::KeyboardInput { event, .. } => {
                let on_button = self.gui.as_ref().is_some_and(Gui::focus_on_button);
                menu_takes_key(
                    self.mode.menu_visible(),
                    event.physical_key,
                    event.repeat,
                    on_button,
                )
            }
            WindowEvent::ModifiersChanged(_) => self.mode.menu_visible(),
            WindowEvent::DroppedFile(_)
            | WindowEvent::HoveredFile(_)
            | WindowEvent::HoveredFileCancelled
            | WindowEvent::RedrawRequested
            | WindowEvent::CloseRequested => false,
            _ => true,
        };
        match (wanted, self.gui.as_mut(), self.window.as_ref()) {
            (true, Some(gui), Some(window)) => gui.event(window, event),
            _ => false,
        }
    }
}

impl<C: Core> ApplicationHandler for App<C> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // The window and the pixel buffer follow the first frame's size; the library
        // has its own window size.
        let first = frame_of(self.test.as_ref(), &self.core);
        let (width, height) = (first.width as u32, first.height as u32);
        self.buffer_size = (width, height);
        if self.mode != Mode::Library {
            self.fitted = Some((width, height));
        }
        let (inner_width, inner_height) = if self.mode == Mode::Library {
            LIBRARY_WINDOW
        } else {
            (width * self.scale, height * self.scale)
        };
        let attrs = Window::default_attributes()
            .with_title(self.title.as_str())
            .with_inner_size(LogicalSize::new(inner_width, inner_height))
            .with_min_inner_size(LogicalSize::new(width, height));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => return self.fail(event_loop, format!("cannot create window: {e}")),
        };
        // Physical pixels: on HiDPI displays this is `scale * logical size`.
        let size = window.inner_size();
        let mut pixels = match crate::surface::create_pixels(width, height, &window) {
            Ok(p) => p,
            Err(e) => return self.fail(event_loop, format!("cannot initialise graphics: {e}")),
        };
        pixels.clear_color(pixels::wgpu::Color::BLACK);
        // The window may already be minimised (zero-sized) when it is created.
        self.surface_ok = surface_extent(size.width, size.height).is_some();
        self.gui = Some(Gui::new(&window, &pixels));
        self.window = Some(window);
        self.pixels = Some(pixels);
        self.update_title();
        self.upload();
        self.reset_clock();
        if self.test.is_none()
            && self.mode == Mode::Playing
            && self.core.system() == emu_common::System::Gb
        {
            log::info!("{}", crate::keymap::SAVE_KEY_NOTICE);
        }
        if let Some(notice) = self.notice.clone() {
            self.toast(log::Level::Warn, notice);
        }
        self.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if self.forward_to_egui(&event) && self.mode.menu_visible() {
            self.request_redraw();
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.resize(size.width, size.height);
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                // The new physical size is delivered via `Resized`, but re-read it
                // here so the surface is right even if that event is coalesced.
                if let Some(size) = self.window.as_ref().map(|w| w.inner_size()) {
                    self.resize(size.width, size.height);
                }
                self.request_redraw();
            }
            WindowEvent::Focused(focused) => {
                if focused {
                    self.reset_clock();
                } else {
                    self.release_all_buttons();
                    self.set_fast_forward(false);
                }
            }
            WindowEvent::Occluded(false) => self.reset_clock(),
            WindowEvent::DroppedFile(path) => self.open_dropped(path),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key
                    && self.on_key_event(code, event.state == ElementState::Pressed, event.repeat)
                        == Effect::Exit
                {
                    event_loop.exit();
                }
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            _ => {}
        }
    }

    /// Woken by the signal handler (also covers the paused/idle loop, which
    /// would otherwise sleep until the next window event).
    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: ()) {
        self.check_shutdown(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Before the `pixels` check: a signal can arrive before the window exists.
        if self.check_shutdown(event_loop) || self.pixels.is_none() {
            return;
        }
        let now = Instant::now();
        let dt = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;
        let status_until = self.expire_status(now);
        let had_toasts = !self.toasts.is_empty();
        let toast_until = self.toasts.expire(now);
        if had_toasts && self.toasts.is_empty() {
            // The last toast ran out: draw the window without it.
            self.request_redraw();
        }

        if self.paused || !self.mode.runs_emulation() {
            // A menu is up (or the library): nothing runs. Wake once more for egui's
            // own animations and to clear a status message or a toast.
            if self.ui_wake.is_some_and(|at| now >= at) {
                self.ui_wake = None;
                self.request_redraw();
            }
            let wake = [status_until, toast_until, self.ui_wake]
                .into_iter()
                .flatten()
                .min();
            event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
            return;
        }
        // While playing egui is not drawn (a toast aside), so what it gathered from the
        // pointer would only pile up.
        if let (Some(gui), Some(window)) = (self.gui.as_mut(), self.window.as_ref()) {
            gui.discard_input(window);
        }
        self.pacer.advance(dt);
        if self.run_due()
            && self.surface_ok
            && let Some(w) = &self.window
        {
            w.request_redraw();
        }
        self.tick_saves(now);

        // Never sleep past the next frame deadline (saves are checked once per wake).
        let wait = self.pacer.time_until_next().max(Duration::from_micros(200));
        event_loop.set_control_flow(ControlFlow::WaitUntil(now + wait));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::save::save_path;
    use emu_common::System;
    use gb_core::{FRAMES_PER_SECOND, GameBoy, M_CYCLES_PER_FRAME, Model};
    use gba_core::Gba;
    use std::fs;

    #[test]
    fn a_failed_buffer_resize_is_not_retried() {
        let (old, new) = ((160, 144), (240, 160));
        assert_eq!(buffer_plan(old, old, false), BufferPlan::Present);
        assert_eq!(buffer_plan(new, old, false), BufferPlan::Resize);
        // After a failure the new size is skipped every frame, not retried...
        assert_eq!(buffer_plan(new, old, true), BufferPlan::Skip);
        // ...and a frame that fits the old buffer still shows.
        assert_eq!(buffer_plan(old, old, true), BufferPlan::Present);
    }

    #[test]
    fn zero_sized_surface_is_not_drawable() {
        assert_eq!(surface_extent(0, 0), None);
        assert_eq!(surface_extent(0, 144), None);
        assert_eq!(surface_extent(160, 0), None);
        assert_eq!(surface_extent(1, 1), Some((1, 1)));
        assert_eq!(surface_extent(640, 576), Some((640, 576)));
    }

    /// A headless `App` (no window, graphics or audio) on a battery cartridge,
    /// saving next to `dir/g.gb`.
    fn headless_app(dir: &std::path::Path) -> App<GameBoy> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x147] = 0x03; // MBC1+RAM+BATTERY
        rom[0x149] = 0x02; // 8 KiB
        headless_app_with(dir, GameBoy::new(rom).unwrap())
    }

    fn headless_app_with(dir: &std::path::Path, mut gb: GameBoy) -> App<GameBoy> {
        let mut saves = SaveManager::new(&save_path(&dir.join("g.gb")), Instant::now());
        saves.load(&mut gb); // no .sav yet: the blank RAM is the baseline
        App::new(
            gb,
            Storage {
                saves,
                states: StateStore::new(GamePaths::beside(&dir.join("g.gb"))),
                paths: GamePaths::beside(&dir.join("g.gb")),
            },
            None,
            [[0; 4]; 4],
            1,
            false,
            ShutdownFlag::default(),
        )
        .with_idle(Box::new(|| GameBoy::new(vec![0u8; 0x8000]).ok()))
    }

    fn change_ram(app: &mut App<GameBoy>, first_byte: u8) {
        let mut ram = app.core.battery_ram().unwrap();
        ram[0] = first_byte;
        app.core.load_battery_ram(&ram).unwrap();
    }

    #[test]
    fn pausing_flushes_battery_ram_but_resuming_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let sav = dir.path().join("g.sav");
        let mut app = headless_app(dir.path());

        change_ram(&mut app, 0xAB);
        app.toggle_pause();
        assert!(app.paused);
        assert_eq!(fs::read(&sav).unwrap()[0], 0xAB, "written on pause");

        change_ram(&mut app, 0xCD);
        app.toggle_pause();
        assert!(!app.paused);
        assert_eq!(fs::read(&sav).unwrap()[0], 0xAB, "not written on resume");
    }

    fn titled_rom(title: &[u8]) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x134..0x134 + title.len()].copy_from_slice(title);
        rom
    }

    #[test]
    fn a_dropped_rom_replaces_the_game_after_saving_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.loader = Some(Box::new(|path| {
            Ok((
                GameBoy::new(titled_rom(b"OTHER")).unwrap(),
                GamePaths::beside(path),
            ))
        }));
        change_ram(&mut app, 0xAB);
        app.frame = 99;
        app.paused = true;

        let other = dir.path().join("other.gb");
        app.open_dropped(other.clone());

        assert_eq!(
            fs::read(dir.path().join("g.sav")).unwrap()[0],
            0xAB,
            "the old game's RAM is written before it is replaced"
        );
        assert_eq!(app.game_paths(), Some(&GamePaths::beside(&other)));
        assert_eq!(app.title, "gb - OTHER");
        assert_eq!((app.frame, app.paused), (0, false));
    }

    #[test]
    fn a_dropped_rom_that_fails_to_load_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.loader = Some(Box::new(|_| {
            Err("cannot read x.gb: file not found".to_owned())
        }));
        app.frame = 7;
        let title = app.title.clone();

        app.open_dropped(dir.path().join("x.gb"));

        assert_eq!(
            app.game_paths(),
            Some(&GamePaths::beside(&dir.path().join("g.gb")))
        );
        assert_eq!((app.title.as_str(), app.frame), (title.as_str(), 7));
        let (msg, _) = app.status.as_ref().expect("the error is shown");
        assert!(msg.contains("file not found"), "{msg}");
        assert!(msg.starts_with("error: "), "{msg}");
    }

    #[test]
    fn a_drop_without_a_loader_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.open_dropped(dir.path().join("x.gb"));
        assert_eq!(
            app.game_paths(),
            Some(&GamePaths::beside(&dir.path().join("g.gb")))
        );
        assert!(app.status.is_none());
    }

    /// MBC3+RTC+RAM+battery ROM whose program loops: select the D-pad, read
    /// P1, write it to BGP. VRAM is blank, so the whole screen's shade follows
    /// Right/Left, and register A follows the latest read.
    fn joypad_to_palette_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x100..0x103].copy_from_slice(&[0xC3, 0x50, 0x01]); // JP 0x0150
        rom[0x147] = 0x10; // MBC3+TIMER+RAM+BATTERY
        rom[0x149] = 0x03; // 32 KiB
        rom[0x150..0x15A].copy_from_slice(&[
            0x3E, 0x20, // LD A,0x20      select the D-pad
            0xE0, 0x00, // LDH (P1),A
            0xF0, 0x00, // LDH A,(P1)
            0xE0, 0x47, // LDH (BGP),A
            0x18, 0xF6, // JR 0x0150
        ]);
        rom
    }

    /// Run `n` emulated frames, as the pacer would.
    fn run_frames(app: &mut App<GameBoy>, n: u32) {
        for _ in 0..n {
            app.step_once();
        }
    }

    /// An `App` that records to `dir/s.gbinput`, powered on as `model` with
    /// whatever `dir/g.sav` holds (the header gets the RAM the save left).
    fn recording_app(dir: &std::path::Path, rom: &[u8], model: Model) -> (App<GameBoy>, PathBuf) {
        use emu_common::replay::rom_hash;

        let rom_path = dir.join("g.gb");
        let mut gb = GameBoy::with_model(rom.to_vec(), model).unwrap();
        gb.set_audio_sample_rate(48_000);
        let mut saves = SaveManager::new(&save_path(&rom_path), Instant::now());
        saves.load(&mut gb);
        let rec_path = dir.join("s.gbinput");
        let header = record::header_for(&gb, rom_hash(rom));
        assert_eq!(header.variant, u8::from(model == Model::Cgb));
        let recorder = Recorder::create(&rec_path, &header).unwrap();
        let app = App::new(
            gb,
            Storage {
                saves,
                states: StateStore::new(GamePaths::beside(&rom_path)),
                paths: GamePaths::beside(&rom_path),
            },
            None,
            [[0; 4]; 4],
            1,
            false,
            ShutdownFlag::default(),
        )
        .with_recorder(Some(recorder));
        (app, rec_path)
    }

    /// Play a scripted session through the key handlers: a press+release inside
    /// one frame, a focus-loss release with two buttons held, then a screenshot.
    /// Returns the screenshot's frame and the total frames run; the recording
    /// is finished.
    fn play_session(app: &mut App<GameBoy>) -> (u64, u64) {
        use winit::keyboard::KeyCode;

        run_frames(app, 2);
        app.on_key_input(KeyCode::ArrowRight, true);
        run_frames(app, 3);
        app.on_key_input(KeyCode::ArrowLeft, true); // press and release in one frame
        app.on_key_input(KeyCode::ArrowLeft, false);
        run_frames(app, 2);
        app.on_key_input(KeyCode::ArrowLeft, true);
        run_frames(app, 1);
        app.release_all_buttons(); // window lost focus with Right and Left held
        app.on_key_input(KeyCode::ArrowRight, true);
        run_frames(app, 1);
        app.on_key_input(KeyCode::KeyC, true); // screenshot + marker
        let marker = app.frame;
        run_frames(app, 4);
        let total = app.frame;
        app.recorder.as_mut().unwrap().finish(total);
        (marker, total)
    }

    /// The one screenshot of `frame` in `dir`, whatever second it was taken in.
    fn screenshot_file(dir: &std::path::Path, frame: u64) -> PathBuf {
        let suffix = format!("-f{frame:07}.png");
        let found: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().ends_with(&suffix))
            .collect();
        assert_eq!(found.len(), 1, "screenshots of frame {frame}: {found:?}");
        found.into_iter().next().unwrap()
    }

    #[test]
    fn a_screenshot_name_carries_the_time_and_does_not_replace_an_earlier_one() {
        use winit::keyboard::KeyCode;

        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.on_key_input(KeyCode::KeyC, true);
        let first = screenshot_file(dir.path(), 0);
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(is_stamped(&name), "{name}");

        // A screenshot of the same frame from an earlier session sits there already.
        let earlier = dir.path().join("g-19990101-000000-f0000000.png");
        fs::write(&earlier, b"old session").unwrap();
        app.on_key_input(KeyCode::KeyC, true);
        assert_eq!(fs::read(&earlier).unwrap(), b"old session");
        assert!(first.exists());
    }

    /// `g-YYYYMMDD-HHMMSS-fNNNNNNN.png`
    fn is_stamped(name: &str) -> bool {
        let b = name.as_bytes();
        name.starts_with("g-")
            && name.ends_with("-f0000000.png")
            && b.len() == "g-20231114-221320-f0000000.png".len()
            && b[2..10].iter().all(u8::is_ascii_digit)
            && b[11..17].iter().all(u8::is_ascii_digit)
    }

    /// Replay `rec` headlessly and require the machine, the colour or shade
    /// frame, and the screenshot file to be identical to the live session's.
    fn assert_replays_identically(
        dir: &std::path::Path,
        rec_path: &std::path::Path,
        rom: Vec<u8>,
        app: &App<GameBoy>,
        (marker, total): (u64, u64),
    ) -> gb_core::replay::Recording {
        use gb_core::replay::{Recording, Replayer};

        let rec = Recording::decode(&fs::read(rec_path).unwrap()).unwrap();
        assert_eq!(rec.end_frame, Some(total));
        assert_eq!(rec.markers(), vec![marker]);
        assert_eq!(
            rec.records.len(),
            8,
            "7 button changes + 1 marker: {:?}",
            rec.records
        );
        assert_eq!(rec.header.model, app.core.model());

        let mut gb = rec.power_on(rom).unwrap();
        assert_eq!(gb.model(), app.core.model());
        gb.set_audio_sample_rate(44_100); // output rate must not affect state
        let mut player = Replayer::new(&rec);
        let mut audio = Vec::new();
        let mut step = |gb: &mut GameBoy, player: &mut Replayer, to: u64| {
            while player.frame() < to {
                player.step_frame(gb);
                audio.clear();
                gb.drain_audio_into(&mut audio);
            }
        };
        step(&mut gb, &mut player, marker);
        let shot = fs::read(screenshot_file(dir, marker)).unwrap();
        let replayed = dir.join("replayed.png");
        record::write_png(gb.frame(), &replayed).unwrap();
        assert_eq!(
            fs::read(&replayed).unwrap(),
            shot,
            "screenshot == replayed frame"
        );
        step(&mut gb, &mut player, total);
        assert_eq!(gb.mcycles(), app.core.mcycles());
        assert_eq!(gb.cpu_registers(), app.core.cpu_registers());
        assert_eq!(
            gb.peek(0xFF0F),
            app.core.peek(0xFF0F),
            "IF (joypad IRQ) matches"
        );
        assert_eq!(gb.framebuffer()[..], app.core.framebuffer()[..]);
        assert_eq!(
            gb.cgb_framebuffer().map(|f| f.to_vec()),
            app.core.cgb_framebuffer().map(|f| f.to_vec()),
            "colour frame matches"
        );
        assert_eq!(
            gb.battery_ram(),
            app.core.battery_ram(),
            "cart RAM + RTC match"
        );
        rec
    }

    /// The whole loop the user relies on: play with `--record`, press the
    /// screenshot key, then replay headlessly and get the identical picture
    /// and machine state. Covers a press+release inside one frame, the
    /// focus-loss release, a short-footer RTC save, and a different audio rate.
    #[test]
    fn recording_replays_to_the_identical_state_and_screenshot() {
        let dir = tempfile::tempdir().unwrap();
        let rom = joypad_to_palette_rom();
        // 32 KiB RAM + the 44-byte RTC footer variant, with recognisable content.
        let mut sav: Vec<u8> = (0..0x8000u32).map(|i| (i * 7) as u8).collect();
        sav.extend_from_slice(&[0; 44]);
        sav[0x8000] = 12; // RTC seconds
        fs::write(dir.path().join("g.sav"), &sav).unwrap();

        let (mut app, rec_path) = recording_app(dir.path(), &rom, Model::Dmg);
        assert_eq!(
            app.core.battery_ram().unwrap()[..0x8000],
            sav[..0x8000],
            "save loaded"
        );
        let session = play_session(&mut app);
        assert_replays_identically(dir.path(), &rec_path, rom, &app, session);
        assert_eq!(
            app.core.framebuffer()[0],
            2,
            "Right held: shade follows the joypad"
        );
    }

    /// A CGB-only ROM that writes the D-pad nibble into BG palette 0 colour 0
    /// (via BCPS/BCPD), so the whole colour frame follows Right/Left.
    fn joypad_to_cgb_palette_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x100..0x103].copy_from_slice(&[0xC3, 0x50, 0x01]); // JP 0x0150
        rom[0x143] = 0xC0; // CGB only
        rom[0x147] = 0x10; // MBC3+TIMER+RAM+BATTERY
        rom[0x149] = 0x03; // 32 KiB
        rom[0x150..0x161].copy_from_slice(&[
            0x3E, 0x20, // LD A,0x20      select the D-pad
            0xE0, 0x00, // LDH (P1),A
            0x3E, 0x80, // LD A,0x80      BCPS: index 0, auto-increment
            0xE0, 0x68, // LDH (BCPS),A
            0xF0, 0x00, // LDH A,(P1)
            0xE0, 0x69, // LDH (BCPD),A   low byte: the D-pad nibble
            0xAF, //       XOR A
            0xE0, 0x69, // LDH (BCPD),A   high byte 0
            0x18, 0xF3, // JR 0x0154
        ]);
        rom
    }

    /// A CGB session must replay as a CGB: the colour frame (not just the
    /// all-zero shade frame) is bit-exact, the screenshot is the RGB picture,
    /// and the recording header carries the model.
    #[test]
    fn cgb_recording_replays_bit_exact_as_cgb() {
        let dir = tempfile::tempdir().unwrap();
        let rom = joypad_to_cgb_palette_rom();
        let (mut app, rec_path) = recording_app(dir.path(), &rom, Model::Cgb);
        let first = *app.core.cgb_framebuffer().expect("colour mode")[..]
            .first()
            .unwrap();
        let session = play_session(&mut app);
        let rec = assert_replays_identically(dir.path(), &rec_path, rom.clone(), &app, session);

        assert_eq!(rec.header.model, Model::Cgb);
        let live = app.core.cgb_framebuffer().expect("colour mode");
        assert_ne!(live[0], first, "the colour frame follows the joypad");
        assert_eq!(live[0] & 0x0F, 0x0E, "Right held: D-pad nibble 1110");
        assert!(
            app.core.framebuffer().iter().all(|&s| s == 0),
            "shades unused"
        );
        // The same file as a DMG would not even be in colour mode.
        let mut dmg = GameBoy::new(rom).unwrap();
        dmg.step_frame();
        assert!(dmg.cgb_framebuffer().is_none());
    }

    #[test]
    fn loading_a_snapshot_stops_the_recording_and_resyncs_through_set_button() {
        use gb_core::replay::{Event, Recording};
        use winit::keyboard::KeyCode;

        let dir = tempfile::tempdir().unwrap();
        let rom = joypad_to_palette_rom();
        let (mut app, rec_path) = recording_app(dir.path(), &rom, Model::Dmg);
        run_frames(&mut app, 2);
        app.on_key_input(KeyCode::ArrowRight, true);
        run_frames(&mut app, 1);
        app.save_snapshot();
        app.on_key_input(KeyCode::ArrowRight, false);
        run_frames(&mut app, 3);
        assert!(app.recorder.is_some(), "saving a snapshot keeps recording");

        // A failed load (no such snapshot) changes nothing and keeps recording.
        assert!(!app.load_snapshot(12345));
        assert!(app.recorder.is_some());

        let frame_at_load = app.frame;
        app.load_latest_snapshot();
        assert!(app.recorder.is_none(), "recording stopped by the load");
        run_frames(&mut app, 2);

        let rec = Recording::decode(&fs::read(&rec_path).unwrap()).unwrap();
        assert_eq!(
            rec.end_frame,
            Some(frame_at_load),
            "ends where the load was"
        );
        assert!(!rec.truncated);
        assert_eq!(
            rec.records.last().unwrap().event,
            Event::Button {
                button: Button::Right,
                pressed: false
            }
        );
    }

    /// L and R exist only on the GBA. Pressing them in a Game Boy session must
    /// neither reach the core nor the recording (whose 8-button decoder would
    /// reject index 8 or 9 and make the whole file unreadable).
    #[test]
    fn l_and_r_never_reach_a_game_boy_recording() {
        use gb_core::replay::Recording;

        let dir = tempfile::tempdir().unwrap();
        let rom = joypad_to_palette_rom();
        let (mut app, rec_path) = recording_app(dir.path(), &rom, Model::Dmg);
        run_frames(&mut app, 1);
        let before = app.core.save_state();
        app.set_button(Button::L, true);
        app.set_button(Button::R, true);
        app.set_button(Button::A, true);
        app.set_button(Button::L, false);
        app.release_all_buttons();
        assert_eq!(app.core.save_state().len(), before.len());
        app.recorder.as_mut().unwrap().finish(app.frame);

        let rec = Recording::decode(&fs::read(&rec_path).unwrap()).expect("still decodable");
        assert_eq!(
            rec.records.iter().map(|r| r.event).collect::<Vec<_>>(),
            vec![Event::Button {
                button: Button::A,
                pressed: true
            }],
            "only the A press was recorded"
        );
    }

    #[test]
    fn the_window_title_is_gb_then_the_game() {
        assert_eq!(window_title("TETRIS"), "gb - TETRIS");
        assert_eq!(window_title(""), "gb");
    }

    /// A GBA ROM that spins on `b .`, titled `GBATEST`, with a Flash ID string.
    fn gba_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x200];
        rom[..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
        rom[0xA0..0xA7].copy_from_slice(b"GBATEST");
        rom[0x100..0x109].copy_from_slice(b"FLASH1M_V");
        rom
    }

    /// A headless `App` (no window, graphics or audio) on any core, saving next
    /// to `dir/g.gba`.
    fn gba_app<C: Core>(dir: &std::path::Path, mut core: C) -> App<C> {
        let mut saves = SaveManager::new(&save_path(&dir.join("g.gba")), Instant::now());
        saves.load(&mut core);
        App::new(
            core,
            Storage {
                saves,
                states: StateStore::new(GamePaths::beside(&dir.join("g.gba"))),
                paths: GamePaths::beside(&dir.join("g.gba")),
            },
            None,
            [[0; 4]; 4],
            1,
            false,
            ShutdownFlag::default(),
        )
    }

    /// The path `main` takes: a GBA core held as `Box<dyn Core>`, stepped through
    /// the trait, with a title, a 240x160 colour frame and a paced step.
    #[test]
    fn a_gba_runs_through_the_app_as_a_boxed_core() {
        let dir = tempfile::tempdir().unwrap();
        let core: Box<dyn Core> = Box::new(Gba::new(gba_rom(), None).unwrap());
        let mut app = gba_app(dir.path(), core);
        assert_eq!(app.title, "gb - GBATEST");
        assert_eq!(app.pacer.period(), 1.0 / gba_core::FRAMES_PER_SECOND);
        for _ in 0..3 {
            let emulated = app.step_once();
            assert!(emulated > 0.0, "a step always covers time: {emulated}");
        }
        assert_eq!(app.frame, 3);
        let f = frame_of(None, &app.core);
        assert_eq!((f.width, f.height), (240, 160));
        assert!(matches!(f.pixels, FramePixels::Rgb555(p) if p.len() == 240 * 160));
        // The upload converts any size: the whole frame fits an RGBA buffer.
        let mut rgba = vec![0u8; 240 * 160 * 4];
        present(&f, &app.palette, &mut rgba);
    }

    /// The files of the snapshots in `dir`'s `g.states` folder, newest first.
    fn snapshot_files(dir: &Path) -> Vec<PathBuf> {
        let paths = GamePaths::beside(&dir.join("g.gb"));
        let secs: Vec<u64> = crate::state::StateStore::new(paths.clone())
            .snapshots()
            .iter()
            .map(|s| s.secs)
            .collect();
        secs.into_iter().map(|s| paths.snapshot_state(s)).collect()
    }

    /// K and L on a GBA, held as the `Box<dyn Core>` `main` builds, write a snapshot
    /// file and restore from it: a state taken at one frame comes back after the
    /// machine has run on.
    #[test]
    fn snapshot_keys_on_a_gba_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let core: Box<dyn Core> = Box::new(Gba::new(gba_rom(), None).unwrap());
        let mut app = gba_app(dir.path(), core);
        app.step_once();
        let snap = app.core.save_state();
        app.save_snapshot();
        assert_eq!(app.status.as_ref().unwrap().0, "Snapshot saved");
        let files = snapshot_files(dir.path());
        assert_eq!(files.len(), 1);
        let saved = fs::read(&files[0]).unwrap();
        assert_eq!(saved, snap);
        assert!(saved.len() <= gba_core::MAX_STATE_BYTES);
        assert!(
            files[0].with_extension("png").is_file(),
            "the picture of the frame is kept"
        );
        for _ in 0..4 {
            app.step_once();
        }
        assert_ne!(app.core.save_state(), snap);
        app.load_latest_snapshot();
        assert!(
            app.status
                .as_ref()
                .unwrap()
                .0
                .starts_with("Loaded snapshot from")
        );
        assert_eq!(app.core.save_state(), snap);
    }

    /// A snapshot of a GBA session started with other options or another BIOS says
    /// what to match, and a corrupt one leaves the machine as it was.
    #[test]
    fn a_gba_snapshot_load_failure_names_the_cause_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let other = Gba::with_options(
            gba_rom(),
            None,
            gba_core::CartOptions {
                save: gba_core::SaveChoice::Sram,
                ..Default::default()
            },
        )
        .unwrap();
        let states = dir.path().join("g.states");
        fs::create_dir_all(&states).unwrap();
        fs::write(states.join("snap-1.state"), other.save_state()).unwrap();
        let mut app = gba_app(dir.path(), Gba::new(gba_rom(), None).unwrap());
        app.step_once();
        let before = app.core.save_state();
        app.load_latest_snapshot();
        let msg = app.status.as_ref().unwrap().0.clone();
        assert!(
            msg.contains("--bios") && msg.contains("--save-type"),
            "{msg}"
        );
        assert_eq!(app.core.save_state(), before);

        let mut bad = before.clone();
        bad.truncate(bad.len() / 2);
        fs::write(states.join("snap-2.state"), bad).unwrap();
        app.load_latest_snapshot();
        assert!(app.status.as_ref().unwrap().0.contains("truncated"));
        assert_eq!(app.core.save_state(), before);
    }

    /// A GBA ROM that copies KEYINPUT into palette entry 0 forever: with no layer
    /// enabled the backdrop shows it, so the picture follows the buttons.
    fn gba_keypad_rom() -> Vec<u8> {
        const AL: u32 = 0xE000_0000;
        let imm = |v: u32| -> u32 {
            (0..16)
                .find_map(|rot| {
                    let r = v.rotate_left(rot * 2);
                    (r <= 0xFF).then_some(rot << 8 | r)
                })
                .expect("encodable immediate")
        };
        let mov = |rd: u32, v: u32| AL | 1 << 25 | 13 << 21 | rd << 12 | imm(v);
        let add = |rd: u32, rn: u32, v: u32| AL | 1 << 25 | 4 << 21 | rn << 16 | rd << 12 | imm(v);
        let half = |load: u32, rd: u32, rn: u32| {
            AL | 1 << 24 | 1 << 23 | 1 << 22 | load << 20 | rn << 16 | rd << 12 | 0xB0
        };
        let words = [
            mov(0, 0x0400_0000),
            add(0, 0, 0x130), // KEYINPUT
            mov(1, 0x0500_0000),
            half(1, 2, 0),                // loop: ldrh r2, [r0]
            half(0, 2, 1),                //       strh r2, [r1]
            AL | 0x0A00_0000 | 0xFF_FFFC, //       b loop (-4 words + 8)
        ];
        let mut rom = vec![0u8; 0x400];
        for (i, w) in words.iter().enumerate() {
            rom[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
        }
        rom
    }

    /// The whole loop for a GBA: play with `--record` (cartridge options, a clock
    /// start and a screenshot included), then replay with `gba_core::replay` and
    /// get the identical machine, the identical screenshot bytes, and the same PNG
    /// `gb-test --replay` writes for that frame.
    #[test]
    fn a_gba_recording_replays_to_the_identical_state_and_screenshot() {
        use emu_common::replay::rom_hash;
        use gba_core::replay::{Recording, Replayer};
        use gba_core::{CartOptions, RtcMode, RtcTime, SaveChoice};
        use winit::keyboard::KeyCode;

        let dir = tempfile::tempdir().unwrap();
        let rom_path = dir.path().join("g.gba");
        let rom = gba_keypad_rom();
        let options = CartOptions {
            save: SaveChoice::Sram,
            rtc: RtcMode::On,
        };
        let start = RtcTime::new(2024, 2, 29, 13, 45, 9).unwrap();
        let mut gba = Gba::with_options(rom.clone(), None, options).unwrap();
        gba.set_rtc(start);
        let mut saves = SaveManager::new(&save_path(&rom_path), Instant::now());
        saves.load(&mut gba);
        let rec_path = dir.path().join("s.gbinput");
        let header = record::header_for(&gba, rom_hash(&rom));
        assert!(!header.ext.is_empty(), "a GBA header carries its options");
        let mut app = App::new(
            gba,
            Storage {
                saves,
                states: StateStore::new(GamePaths::beside(&rom_path)),
                paths: GamePaths::beside(&rom_path),
            },
            None,
            [[0; 4]; 4],
            1,
            false,
            ShutdownFlag::default(),
        )
        .with_recorder(Some(Recorder::create(&rec_path, &header).unwrap()));

        let frames = |app: &mut App<Gba>, n: u32| (0..n).for_each(|_| _ = app.step_once());
        frames(&mut app, 2);
        app.on_key_input(KeyCode::ArrowRight, true);
        frames(&mut app, 3);
        app.on_key_input(KeyCode::ArrowLeft, true); // press and release in one frame
        app.on_key_input(KeyCode::ArrowLeft, false);
        frames(&mut app, 2);
        app.on_key_input(KeyCode::KeyR, true); // the R shoulder
        frames(&mut app, 1);
        app.release_all_buttons();
        app.on_key_input(KeyCode::ArrowRight, true);
        frames(&mut app, 1);
        app.on_key_input(KeyCode::KeyC, true); // screenshot + marker
        let marker = app.frame;
        frames(&mut app, 4);
        let total = app.frame;
        app.recorder.as_mut().unwrap().finish(total);

        let rec = Recording::decode(&fs::read(&rec_path).unwrap()).unwrap();
        assert_eq!(rec.end_frame, Some(total));
        assert_eq!(rec.markers(), vec![marker]);
        assert_eq!(rec.header.options, options);
        assert_eq!(rec.header.rtc_start, Some(start));
        assert_eq!(rec.header.bios_hash, None);

        let mut gba = rec.power_on(rom, None).unwrap();
        let mut player = Replayer::new(&rec);
        let mut audio = Vec::new();
        let mut step = |gba: &mut Gba, to: u64| {
            while player.frame() < to {
                player.step_frame(gba);
                audio.clear();
                gba.drain_audio_into(&mut audio);
            }
        };
        step(&mut gba, marker);
        let shot = fs::read(screenshot_file(dir.path(), marker)).unwrap();
        let replayed = dir.path().join("replayed.png");
        record::write_png(Core::frame(&gba), &replayed).unwrap();
        assert_eq!(
            fs::read(&replayed).unwrap(),
            shot,
            "screenshot == replayed frame"
        );
        let tool = dir.path().join("gb-test.png");
        gb_test::gba::save_gba_png(gba.framebuffer(), &tool).unwrap();
        assert_eq!(
            fs::read(&tool).unwrap(),
            shot,
            "gb-test's PNG == the frontend's screenshot, byte for byte"
        );
        step(&mut gba, total);
        assert_eq!(gba.cycles(), app.core.cycles());
        assert_eq!(gba.cpu_registers(), app.core.cpu_registers());
        assert_eq!(gba.framebuffer()[..], app.core.framebuffer()[..]);
        assert_eq!(gba.save_state(), app.core.save_state(), "the whole machine");
        assert_eq!(
            gba.framebuffer()[0] & 0x3FF,
            0x3FF & !0x010,
            "Right held: the palette followed KEYINPUT"
        );
    }

    /// L and R (Q and R on the keyboard) reach a GBA and its recording, which
    /// its 10-button decoder reads back, with a 128 KiB flash save in the header.
    #[test]
    fn l_and_r_reach_a_gba_and_its_recording() {
        use emu_common::replay::{decode_header, decode_records, rom_hash};
        use winit::keyboard::KeyCode;

        let dir = tempfile::tempdir().unwrap();
        let rom_path = dir.path().join("g.gba");
        let rom = gba_rom();
        let mut gba = Gba::new(rom.clone(), None).unwrap();
        let mut saves = SaveManager::new(&save_path(&rom_path), Instant::now());
        saves.load(&mut gba);
        let rec_path = dir.path().join("s.gbinput");
        let header = record::header_for(&gba, rom_hash(&rom));
        assert_eq!(header.system, System::Gba);
        assert_eq!(header.battery.as_ref().map(Vec::len), Some(128 * 1024));
        let mut app = App::new(
            gba,
            Storage {
                saves,
                states: StateStore::new(GamePaths::beside(&rom_path)),
                paths: GamePaths::beside(&rom_path),
            },
            None,
            [[0; 4]; 4],
            1,
            false,
            ShutdownFlag::default(),
        )
        .with_recorder(Some(Recorder::create(&rec_path, &header).unwrap()));

        const KEYINPUT: u32 = 0x0400_0130;
        app.step_once();
        app.on_key_input(KeyCode::KeyQ, true);
        app.on_key_input(KeyCode::KeyR, true);
        app.step_once();
        assert_eq!(!app.core.peek16(KEYINPUT) & 0x3FF, 0x300, "L and R down");
        app.on_key_input(KeyCode::KeyQ, false);
        app.release_all_buttons();
        assert_eq!(app.core.peek16(KEYINPUT) & 0x3FF, 0x3FF);
        let total = app.frame;
        app.recorder.as_mut().unwrap().finish(total);

        let bytes = fs::read(&rec_path).unwrap();
        let (h, rest) = decode_header(&bytes).unwrap();
        assert_eq!((h.system, h.variant), (System::Gba, 0));
        assert_eq!(h.battery.map(|b| b.len()), Some(128 * 1024));
        let recs = decode_records(rest, System::Gba.button_count()).unwrap();
        assert_eq!(recs.end_frame, Some(total));
        let events: Vec<_> = recs.records.iter().map(|r| r.event).collect();
        assert_eq!(
            events,
            vec![
                Event::Button {
                    button: Button::L,
                    pressed: true
                },
                Event::Button {
                    button: Button::R,
                    pressed: true
                },
                Event::Button {
                    button: Button::L,
                    pressed: false
                },
                Event::Button {
                    button: Button::R,
                    pressed: false
                },
            ]
        );
    }

    #[test]
    fn a_normal_step_covers_one_frame_of_emulated_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        let period = app.pacer.period();
        app.step_once(); // the first frame starts mid-frame after the boot state
        let total: f64 = (0..120).map(|_| app.step_once()).sum();
        // Samples are whole, so the total is within a sample of 120 periods.
        let rate = f64::from(app.core.audio_sample_rate());
        assert!((total - 120.0 * period).abs() < 2.0 / rate, "{total}");
    }

    /// A CGB-only ROM that switches to double speed once (`LD A,1; LDH
    /// (KEY1),A; STOP`) and then spins.
    fn speed_switch_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x143] = 0xC0;
        rom[0x100..0x103].copy_from_slice(&[0xC3, 0x50, 0x01]); // JP 0x150
        let code = [0x3E, 0x01, 0xE0, 0x4D, 0x10, 0x00, 0x18, 0xFE];
        rom[0x150..0x150 + code.len()].copy_from_slice(&code);
        rom
    }

    #[test]
    fn a_step_spanning_a_speed_switch_is_charged_its_real_time() {
        let dir = tempfile::tempdir().unwrap();
        let make = || GameBoy::with_model(speed_switch_rom(), Model::Cgb).unwrap();
        // Find the M-cycle the STOP starts at, on a second identical machine.
        let mut probe = make();
        let mut switch_at = probe.mcycles();
        while !probe.double_speed() {
            switch_at = probe.mcycles();
            probe.step_instruction();
        }

        let mut app = headless_app_with(dir.path(), make());
        let m_per_s = f64::from(M_CYCLES_PER_FRAME) * FRAMES_PER_SECOND;
        let period = app.pacer.period();
        let rate = f64::from(app.core.audio_sample_rate());
        for _ in 0..4 {
            let start = app.core.mcycles();
            let was_double = app.core.double_speed();
            let emulated = app.step_once();
            if was_double || !app.core.double_speed() {
                continue;
            }
            // Pre-switch cycles take 1/1048576 s each, the rest half that.
            let end = app.core.mcycles();
            let real = ((switch_at - start) as f64 + (end - switch_at) as f64 / 2.0) / m_per_s;
            assert!((emulated - real).abs() < 3.0 / rate, "{emulated} vs {real}");
            // Not vacuous: counting every M-cycle at the normal rate is far off.
            let naive = (end - start) as f64 / m_per_s;
            assert!(naive - real > 0.4 * period, "{naive} vs {real}");
            return;
        }
        panic!("no step contained the speed switch");
    }

    #[test]
    fn run_due_runs_the_steps_the_pacer_owes() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        assert!(!app.run_due(), "nothing owed yet");
        let before = app.core.mcycles();
        app.pacer
            .advance(Duration::from_secs_f64(2.5 * app.pacer.period()));
        assert!(app.run_due());
        let frames = (app.core.mcycles() - before) as f64 / f64::from(M_CYCLES_PER_FRAME);
        assert!((1.9..=3.1).contains(&frames), "{frames}");
        assert!(!app.pacer.frame_due(), "the debt is paid");
        // A long stall is bounded by the catch-up cap.
        app.pacer.advance(Duration::from_secs(60));
        let before = app.core.mcycles();
        assert!(app.run_due());
        let frames = (app.core.mcycles() - before) as f64 / f64::from(M_CYCLES_PER_FRAME);
        assert!(
            frames <= f64::from(crate::pacer::MAX_CATCHUP_FRAMES) + 1.1,
            "{frames}"
        );
    }

    #[test]
    fn k_and_l_save_and_load_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.core.step_frame();
        let snap = app.core.save_state();
        app.on_key(KeyCode::KeyK, true);
        assert_eq!(snapshot_files(dir.path()).len(), 1);
        assert_eq!(app.status.as_ref().unwrap().0, "Snapshot saved");
        assert!(app.toasts.iter().any(|t| t.text == "Snapshot saved"));

        for _ in 0..5 {
            app.step_once();
        }
        app.on_key(KeyCode::KeyL, true);
        assert_eq!(app.core.save_state(), snap);
        assert!(app.toasts.iter().any(|t| {
            t.text.starts_with("Loaded snapshot from")
                && t.text
                    .ends_with("your previous state is kept as 'Before last load'")
        }));
    }

    /// A press of K that catches L too (9 ms apart) saves and loads, and the load can be
    /// undone: where the game was is in the list as "Before last load".
    #[test]
    fn a_load_can_be_undone_from_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.core.step_frame();
        app.on_key(KeyCode::KeyK, true);
        for _ in 0..3 {
            app.step_once();
        }
        let left = app.core.save_state();
        app.on_key(KeyCode::KeyL, true);
        assert_ne!(app.core.save_state(), left);
        let paths = GamePaths::beside(&dir.path().join("g.gb"));
        assert_eq!(fs::read(paths.before_load_state()).unwrap(), left);
        assert!(
            paths.before_load_image().is_file(),
            "with the picture on screen"
        );
        assert_eq!(snapshot_files(dir.path()).len(), 1, "not one more snapshot");

        app.on_key(KeyCode::Escape, true);
        assert!(app.snapshot_list[0].before_load, "listed first");
        assert_eq!(app.snapshot_list.len(), 2);
        app.perform(UiAction::LoadBeforeLoad);
        assert_eq!(app.core.save_state(), left, "back where it was");
        assert_eq!(app.mode, Mode::Playing);
        assert!(
            app.toasts
                .iter()
                .any(|t| t.text.starts_with("Back to before the last load"))
        );
    }

    /// Holding K or L down (the window repeating the press) acts once; and a key whose
    /// release never came (lost under Cmd on macOS) still acts on its next press.
    #[test]
    fn a_held_snapshot_key_acts_once_and_a_lost_release_does_not_disable_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.core.step_frame();
        app.on_key_event(KeyCode::KeyK, true, false);
        for _ in 0..3 {
            app.on_key_event(KeyCode::KeyK, true, true);
        }
        assert_eq!(snapshot_files(dir.path()).len(), 1, "one save while held");
        // No release: the next press is a press all the same.
        app.on_key_event(KeyCode::KeyK, true, false);
        assert_eq!(
            snapshot_files(dir.path()).len(),
            2,
            "pressed again: saves again"
        );

        let saved = app.core.save_state();
        for _ in 0..3 {
            app.step_once();
        }
        app.on_key_event(KeyCode::KeyL, true, false);
        assert_eq!(app.core.save_state(), saved, "L loads");
        for _ in 0..3 {
            app.step_once();
        }
        let moved = app.core.save_state();
        assert_ne!(moved, saved);
        for _ in 0..3 {
            app.on_key_event(KeyCode::KeyL, true, true);
        }
        assert_eq!(app.core.save_state(), moved, "its repeats load nothing");
        app.on_key_event(KeyCode::KeyL, true, false);
        assert_eq!(
            app.core.save_state(),
            saved,
            "a lost release does not stop the next L"
        );
    }

    #[test]
    fn l_loads_the_latest_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.core.step_frame();
        app.save_snapshot();
        for _ in 0..3 {
            app.core.step_frame();
        }
        let latest = app.core.save_state();
        app.save_snapshot();
        assert_eq!(snapshot_files(dir.path()).len(), 2);
        for _ in 0..5 {
            app.core.step_frame();
        }
        app.load_latest_snapshot();
        assert_eq!(app.core.save_state(), latest);
        // The toast names the time, as the list does.
        let latest = app.snapshot_list.iter().find(|s| !s.before_load).unwrap();
        let said = crate::ui::friendly_time_local(latest.secs);
        assert!(app.toasts.iter().any(|t| t.text.contains(&said)), "{said}");
    }

    #[test]
    fn l_with_no_snapshots_says_so_and_keeps_the_machine() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.core.step_frame();
        let before = app.core.save_state();
        app.on_key(KeyCode::KeyL, true);
        assert_eq!(app.core.save_state(), before);
        assert!(app.toasts.iter().any(|t| t.text == "No snapshots yet"));
        assert!(
            !dir.path().join("g.states").exists(),
            "nothing written, no before-load slot either"
        );
    }

    #[test]
    fn snapshots_are_deleted_from_the_list_and_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.save_snapshot();
        app.save_snapshot();
        app.on_key(KeyCode::Escape, true);
        assert_eq!(app.snapshot_list.len(), 2, "read when the menu opens");
        let oldest = app.snapshot_list[1].clone();
        app.perform(UiAction::DeleteSnapshot(oldest.secs));
        assert_eq!(app.snapshot_list.len(), 1);
        assert_eq!(snapshot_files(dir.path()).len(), 1);
        assert!(
            !dir.path()
                .join(format!("g.states/snap-{}.png", oldest.secs))
                .exists()
        );
    }

    #[test]
    fn the_snapshot_list_follows_the_disk_after_a_failed_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.save_snapshot();
        app.save_snapshot();
        app.on_key(KeyCode::Escape, true);
        assert_eq!(app.snapshot_list.len(), 2);
        // One goes missing behind the app's back; loading it fails and the list notices.
        let gone = app.snapshot_list[0].secs;
        fs::remove_file(dir.path().join(format!("g.states/snap-{gone}.state"))).unwrap();
        app.perform(UiAction::LoadSnapshot(gone));
        assert_eq!(app.mode, Mode::Paused);
        assert!(app.toasts.iter().any(|t| t.level == log::Level::Warn));
        assert_eq!(app.snapshot_list.len(), 1);
        assert!(app.snapshot_list.iter().all(|s| s.secs != gone));
        // The same for a delete of one that is already gone.
        let other = app.snapshot_list[0].secs;
        fs::remove_file(dir.path().join(format!("g.states/snap-{other}.state"))).unwrap();
        app.perform(UiAction::DeleteSnapshot(other));
        assert!(app.snapshot_list.is_empty());
    }

    #[test]
    fn the_library_marks_resumable_games_when_it_is_entered_or_changed_not_when_drawn() {
        let dir = tempfile::tempdir().unwrap();
        let (library, id, _) = library_with_a_game(dir.path());
        let paths = library.paths_for(&id);
        let mut app = headless_app(dir.path())
            .with_library(Some(library))
            .with_game(Some(id.clone()));
        assert!(app.resumable.is_empty());
        fs::create_dir_all(paths.states_dir()).unwrap();
        fs::write(paths.resume_state(), b"x").unwrap();
        assert!(app.resumable.is_empty(), "nothing reads the disk by itself");

        app.perform(UiAction::BackToLibrary);
        assert!(app.resumable.contains(&id), "read on entering the library");
        fs::remove_file(paths.resume_state()).unwrap();
        assert!(app.resumable.contains(&id), "not re-read while it is shown");

        app.perform(UiAction::Remove {
            id,
            delete_saves: true,
        });
        assert!(
            app.resumable.is_empty(),
            "read again once the library changed"
        );
    }

    #[test]
    fn the_snapshot_cap_removes_the_oldest_and_a_toast_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        for _ in 0..crate::state::MAX_SNAPSHOTS {
            app.save_snapshot();
        }
        assert!(app.toasts.iter().all(|t| !t.text.contains("oldest")));
        let first = snapshot_files(dir.path()).last().cloned().unwrap();
        app.save_snapshot();
        assert_eq!(
            snapshot_files(dir.path()).len(),
            crate::state::MAX_SNAPSHOTS
        );
        assert!(!first.exists(), "the oldest went");
        assert!(
            app.toasts
                .iter()
                .any(|t| t.text.contains("oldest one was removed"))
        );
    }

    #[test]
    fn a_snapshot_chosen_in_the_list_loads_and_the_game_carries_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.core.step_frame();
        let snap = app.core.save_state();
        app.save_snapshot();
        app.on_key(KeyCode::Escape, true);
        for _ in 0..5 {
            app.core.step_frame();
        }
        let secs = app.snapshot_list[0].secs;
        app.perform(UiAction::LoadSnapshot(secs));
        assert_eq!(app.core.save_state(), snap);
        assert_eq!(app.mode, Mode::Playing, "the menu is left");
        // One that is gone is reported and the menu stays.
        app.on_key(KeyCode::Escape, true);
        app.perform(UiAction::LoadSnapshot(secs + 99));
        assert_eq!(app.mode, Mode::Paused);
        assert!(app.toasts.iter().any(|t| t.level == log::Level::Warn));
    }

    #[test]
    fn loading_a_snapshot_rewinds_the_battery_save_after_backing_it_up() {
        let dir = tempfile::tempdir().unwrap();
        let sav = dir.path().join("g.sav");
        let mut app = headless_app(dir.path());
        app.save_snapshot(); // blank RAM
        change_ram(&mut app, 0xAB);
        app.toggle_pause(); // writes 0xAB to g.sav
        app.load_latest_snapshot();
        app.toggle_pause();
        app.toggle_pause(); // pause flushes the restored RAM
        assert_eq!(
            fs::read(&sav).unwrap()[0],
            0x00,
            "rewound with the snapshot"
        );
        assert_eq!(fs::read(dir.path().join("g.sav.bak")).unwrap()[0], 0xAB);
        // The toast and the title say when; the backup path is for the log only.
        let said = app
            .toasts
            .iter()
            .find(|t| t.text.starts_with("Loaded"))
            .unwrap();
        assert_eq!(
            said.text,
            format!(
                "Loaded snapshot from {} \u{2014} your previous state is kept as 'Before last load'",
                crate::ui::friendly_time_local(
                    app.snapshot_list
                        .iter()
                        .find(|s| !s.before_load)
                        .unwrap()
                        .secs
                )
            )
        );
        assert!(
            app.status
                .as_ref()
                .is_some_and(|(t, _)| !t.contains(".bak"))
        );
    }

    #[test]
    fn status_message_expires() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.show_status(log::Level::Info, "hi".into());
        let until = app.expire_status(Instant::now()).unwrap();
        assert!(app.status.is_some());
        assert_eq!(app.expire_status(until), None);
        assert!(app.status.is_none());
    }

    #[test]
    fn pausing_without_a_ram_change_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.toggle_pause();
        assert!(!dir.path().join("g.sav").exists());
    }

    // --- screens: modes, Esc, menus, the library -------------------------------------

    use winit::keyboard::KeyCode;

    /// The joypad events a finished recording holds.
    fn button_events(app: &mut App<GameBoy>, rec_path: &Path) -> Vec<(Button, bool)> {
        use gb_core::replay::Recording;

        app.recorder.as_mut().unwrap().finish(app.frame);
        let rec = Recording::decode(&fs::read(rec_path).unwrap()).unwrap();
        rec.records
            .iter()
            .filter_map(|r| match r.event {
                Event::Button { button, pressed } => Some((button, pressed)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn esc_quits_from_the_library() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.set_mode(Mode::Library);
        assert_eq!(app.on_key(KeyCode::Escape, true), Effect::Exit);
        // The release of the key does nothing.
        assert_eq!(app.on_key(KeyCode::Escape, false), Effect::None);
    }

    #[test]
    fn esc_pauses_a_running_game_and_resumes_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        assert_eq!(app.mode, Mode::Playing);
        assert_eq!(app.on_key(KeyCode::Escape, true), Effect::None);
        assert_eq!((app.mode, app.paused), (Mode::Paused, true));
        assert_eq!(app.on_key(KeyCode::Escape, true), Effect::None);
        assert_eq!((app.mode, app.paused), (Mode::Playing, false));
    }

    #[test]
    fn p_opens_and_closes_the_same_menu_and_does_nothing_in_the_library() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.on_key(KeyCode::KeyP, true);
        assert_eq!(app.mode, Mode::Paused);
        app.on_key(KeyCode::KeyP, true);
        assert_eq!(app.mode, Mode::Playing);
        app.set_mode(Mode::Library);
        app.on_key(KeyCode::KeyP, true);
        assert_eq!(app.mode, Mode::Library);
    }

    #[test]
    fn game_input_does_not_reach_the_core_while_a_menu_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let rom = joypad_to_palette_rom();
        let (mut app, rec_path) = recording_app(dir.path(), &rom, Model::Dmg);
        app.on_key(KeyCode::Escape, true);
        assert_eq!(app.mode, Mode::Paused);

        // In the pause menu: a button, the snapshot keys and the screenshot key.
        for key in [
            KeyCode::ArrowRight,
            KeyCode::KeyK,
            KeyCode::KeyL,
            KeyCode::KeyC,
        ] {
            app.on_key(key, true);
        }
        assert!(
            snapshot_files(dir.path()).is_empty(),
            "K does not save a snapshot over the menu"
        );
        assert!(
            fs::read_dir(dir.path()).unwrap().all(|e| e
                .unwrap()
                .path()
                .extension()
                .is_none_or(|x| x != "png")),
            "C does not take a screenshot over the menu"
        );
        // Nor does the Library take them.
        app.set_mode(Mode::Library);
        app.on_key(KeyCode::KeyZ, true);

        assert_eq!(button_events(&mut app, &rec_path), vec![]);
    }

    #[test]
    fn a_key_held_through_the_menu_is_pressed_again_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        let rom = joypad_to_palette_rom();
        let (mut app, rec_path) = recording_app(dir.path(), &rom, Model::Dmg);
        app.on_key(KeyCode::ArrowLeft, true); // held before the menu
        app.on_key(KeyCode::Escape, true);
        app.on_key(KeyCode::ArrowLeft, false); // let go over the menu
        app.on_key(KeyCode::ArrowRight, true); // pressed over the menu
        app.on_key(KeyCode::Escape, true);
        assert_eq!(app.mode, Mode::Playing);
        assert_eq!(
            button_events(&mut app, &rec_path),
            vec![
                (Button::Left, true),
                (Button::Left, false), // let go when the menu opened
                (Button::Right, true), // still held when it closed
            ]
        );
    }

    #[test]
    fn fast_forward_ends_when_tab_is_released_over_a_menu() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.on_key(KeyCode::Tab, true);
        assert!(app.fast_forward);
        app.on_key(KeyCode::Escape, true);
        app.on_key(KeyCode::Tab, false);
        assert!(!app.fast_forward);
        app.on_key(KeyCode::Tab, true);
        assert!(!app.fast_forward, "Tab over a menu does not start it");
    }

    #[test]
    fn back_to_library_writes_the_save_and_leaves_the_game() {
        let dir = tempfile::tempdir().unwrap();
        let sav = dir.path().join("g.sav");
        let mut app = headless_app(dir.path());
        app.on_key(KeyCode::Escape, true);
        change_ram(&mut app, 0xAB); // since the pause wrote the save
        app.perform(UiAction::BackToLibrary);
        assert_eq!(fs::read(&sav).unwrap()[0], 0xAB);
        assert_eq!((app.mode, app.paused), (Mode::Library, true));
        assert_eq!(app.title, "gb");
        assert!(app.game_id.is_none());
    }

    #[test]
    fn problems_are_toasted_and_so_is_everything_over_a_menu() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.show_status(log::Level::Info, "frame skipped".into());
        assert!(app.toasts.is_empty(), "plain feedback stays in the title");
        app.show_status(log::Level::Warn, "cannot write snapshot".into());
        app.show_status(log::Level::Error, "bad ROM".into());
        assert_eq!(app.toasts.iter().count(), 2);
        app.on_key(KeyCode::Escape, true);
        app.show_status(log::Level::Info, "Screenshot saved".into());
        assert!(app.toasts.iter().any(|t| t.text == "Screenshot saved"));
    }

    /// A library in `dir` holding one game, `ONE`, and its id.
    fn library_with_a_game(dir: &Path) -> (Library, GameId, PathBuf) {
        use crate::paths::DataDir;

        let data = DataDir::at(dir.join("data")).created().unwrap();
        let mut library = Library::open(&data);
        let rom = dir.join("one.gb");
        fs::write(&rom, titled_rom(b"ONE")).unwrap();
        let added = library.add_rom(&rom).unwrap();
        (library, added.entry.id, rom)
    }

    /// A ROM that keeps writing a line of tile data, so the screen is not one colour.
    fn drawing_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x100..0x103].copy_from_slice(&[0xC3, 0x50, 0x01]); // JP 0x0150
        rom[0x150..0x157].copy_from_slice(&[
            0x3E, 0xFF, // LD A,0xFF
            0xEA, 0x00, 0x80, // LD (0x8000),A
            0x18, 0xF9, // JR 0x0150
        ]);
        rom
    }

    fn thumbnail_of(app: &App<GameBoy>, id: &GameId) -> Option<PathBuf> {
        app.library.as_ref()?.get(id)?.thumbnail.clone()
    }

    fn drawing_app(dir: &Path) -> (App<GameBoy>, GameId) {
        let (library, id, _) = library_with_a_game(dir);
        let app = headless_app_with(dir, GameBoy::new(drawing_rom()).unwrap())
            .with_library(Some(library))
            .with_game(Some(id.clone()));
        (app, id)
    }

    #[test]
    fn a_game_gets_its_thumbnail_once_after_ten_seconds() {
        use crate::ui::THUMBNAIL_FRAMES;

        let dir = tempfile::tempdir().unwrap();
        let (mut app, id) = drawing_app(dir.path());

        run_frames(&mut app, (THUMBNAIL_FRAMES - 1) as u32);
        assert!(thumbnail_of(&app, &id).is_none(), "not yet");
        run_frames(&mut app, 1);
        let path = thumbnail_of(&app, &id).expect("taken at the threshold");
        let png = fs::read(&path).unwrap();
        assert_eq!(
            png,
            app.library.as_ref().unwrap().read_thumbnail(&id).unwrap()
        );

        // Once only: a later change to the file is not overwritten.
        fs::write(&path, b"kept").unwrap();
        run_frames(&mut app, (THUMBNAIL_FRAMES * 2) as u32);
        assert_eq!(fs::read(&path).unwrap(), b"kept");
    }

    #[test]
    fn fast_forward_does_not_count_towards_the_thumbnail() {
        use crate::ui::THUMBNAIL_FRAMES;

        let dir = tempfile::tempdir().unwrap();
        let (mut app, id) = drawing_app(dir.path());
        app.fast_forward = true;
        run_frames(&mut app, (THUMBNAIL_FRAMES * 2) as u32);
        assert!(thumbnail_of(&app, &id).is_none());
        app.fast_forward = false;
        run_frames(&mut app, THUMBNAIL_FRAMES as u32);
        assert!(thumbnail_of(&app, &id).is_some());
    }

    #[test]
    fn a_blank_screen_gets_no_thumbnail_and_the_tries_stop() {
        use crate::ui::THUMBNAIL_GIVE_UP;

        let dir = tempfile::tempdir().unwrap();
        let (library, id, _) = library_with_a_game(dir.path());
        // The blank cartridge draws nothing but one colour.
        let mut app = headless_app(dir.path())
            .with_library(Some(library))
            .with_game(Some(id.clone()));
        run_frames(&mut app, (THUMBNAIL_GIVE_UP + 100) as u32);
        assert!(thumbnail_of(&app, &id).is_none());
    }

    #[test]
    fn a_game_that_has_a_thumbnail_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let (mut library, id, _) = library_with_a_game(dir.path());
        let existing = {
            let frame = FrameRef {
                width: 2,
                height: 2,
                pixels: FramePixels::Shades(&[0, 1, 2, 3]),
            };
            record::encode_png(frame).unwrap()
        };
        library.set_thumbnail(&id, &existing).unwrap();
        let mut app = headless_app_with(dir.path(), GameBoy::new(drawing_rom()).unwrap())
            .with_library(Some(library))
            .with_game(Some(id.clone()));
        run_frames(&mut app, 700);
        assert_eq!(
            app.library.as_ref().unwrap().read_thumbnail(&id).unwrap(),
            existing
        );
    }

    #[test]
    fn a_game_not_in_the_library_has_no_thumbnail_and_no_id() {
        let dir = tempfile::tempdir().unwrap();
        let (library, _, _) = library_with_a_game(dir.path());
        let other = GameId::new("OTHER", 7);
        let mut app = headless_app(dir.path())
            .with_library(Some(library))
            .with_game(Some(other));
        assert!(app.game_id.is_none());
        run_frames(&mut app, 700);
        assert!(
            app.library.as_ref().unwrap().entries()[0]
                .thumbnail
                .is_none()
        );
    }

    #[test]
    fn playing_a_library_game_marks_it_played() {
        let dir = tempfile::tempdir().unwrap();
        let (library, id, _) = library_with_a_game(dir.path());
        let mut app = headless_app(dir.path()).with_library(Some(library));
        assert!(
            app.library
                .as_ref()
                .unwrap()
                .get(&id)
                .unwrap()
                .last_played
                .is_none()
        );
        app.loader = Some(Box::new(|path| {
            Ok((
                GameBoy::new(titled_rom(b"ONE")).unwrap(),
                GamePaths::beside(path),
            ))
        }));
        app.set_mode(Mode::Library);

        app.perform(UiAction::Play(id.clone()));

        assert_eq!((app.mode, app.paused), (Mode::Playing, false));
        assert_eq!(app.game_id.as_ref(), Some(&id));
        assert!(
            app.library
                .as_ref()
                .unwrap()
                .get(&id)
                .unwrap()
                .last_played
                .is_some()
        );
    }

    #[test]
    fn a_dropped_rom_is_added_to_the_library_and_played_from_its_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (library, _, _) = library_with_a_game(dir.path());
        let mut app = headless_app(dir.path()).with_library(Some(library));
        app.loader = Some(Box::new(|path| {
            Ok((
                GameBoy::new(titled_rom(b"TWO")).unwrap(),
                GamePaths::beside(path),
            ))
        }));
        app.set_mode(Mode::Library);
        let dropped = dir.path().join("two.gb");
        fs::write(&dropped, titled_rom(b"TWO")).unwrap();

        app.open_dropped(dropped.clone());

        let library = app.library.as_ref().unwrap();
        assert_eq!(library.entries().len(), 2);
        assert_eq!(app.mode, Mode::Playing);
        let id = app
            .game_id
            .clone()
            .expect("the dropped game is a library game");
        let entry = library.get(&id).unwrap();
        assert_eq!(entry.title, "TWO");
        assert_ne!(entry.rom_path, dropped, "listed by its library copy");
        assert!(entry.last_played.is_some());
        assert!(app.toasts.iter().any(|t| t.text == "Added TWO"));

        // The same file again is reported, not added twice.
        app.set_mode(Mode::Library);
        app.open_dropped(dropped);
        assert_eq!(app.library.as_ref().unwrap().entries().len(), 2);
        assert!(
            app.toasts
                .iter()
                .any(|t| t.text == "TWO is already in the library")
        );
    }

    #[test]
    fn a_drop_that_does_not_power_on_is_not_added_to_the_library() {
        let dir = tempfile::tempdir().unwrap();
        let (library, _, _) = library_with_a_game(dir.path());
        let mut app = headless_app(dir.path()).with_library(Some(library));
        app.loader = Some(Box::new(|_| Err("not a ROM".to_owned())));
        app.set_mode(Mode::Library);
        let dropped = dir.path().join("new.gb");
        fs::write(&dropped, titled_rom(b"NEW")).unwrap();

        app.open_dropped(dropped);

        assert_eq!(app.mode, Mode::Library);
        assert_eq!(app.library.as_ref().unwrap().entries().len(), 1);
        assert!(app.toasts.iter().any(|t| t.level == log::Level::Error));
    }

    #[test]
    fn a_drop_that_plays_but_cannot_be_added_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (library, _, _) = library_with_a_game(dir.path());
        let mut app = headless_app(dir.path()).with_library(Some(library));
        // The loader takes it; the library's own checks do not.
        app.loader = Some(Box::new(|path| {
            Ok((
                GameBoy::new(titled_rom(b"ODD")).unwrap(),
                GamePaths::beside(path),
            ))
        }));
        app.set_mode(Mode::Library);
        let dropped = dir.path().join("odd.gb");
        fs::write(&dropped, b"too small").unwrap();

        app.open_dropped(dropped);

        assert_eq!(app.mode, Mode::Playing);
        assert!(app.game_id.is_none());
        assert_eq!(app.library.as_ref().unwrap().entries().len(), 1);
        assert!(app.toasts.iter().any(|t| t.level == log::Level::Warn));
    }

    #[test]
    fn a_game_whose_file_is_missing_is_not_started() {
        let dir = tempfile::tempdir().unwrap();
        let (library, id, _) = library_with_a_game(dir.path());
        let rom = library.get(&id).unwrap().rom_path.clone();
        fs::remove_file(rom).unwrap();
        // A fresh read of the folder sees the file gone.
        let library = Library::open(library.data_dir());
        let mut app = headless_app(dir.path()).with_library(Some(library));
        app.loader = Some(Box::new(|_| panic!("a missing game must not be loaded")));
        app.set_mode(Mode::Library);
        app.perform(UiAction::Play(id));
        assert_eq!(app.mode, Mode::Library);
        assert!(app.toasts.iter().any(|t| t.text.contains("missing")));
    }

    #[test]
    fn removing_a_game_keeps_its_saves_unless_asked() {
        for delete_saves in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (library, id, _) = library_with_a_game(dir.path());
            let paths = library.paths_for(&id);
            let sav = paths.sav.clone();
            fs::write(&sav, b"progress").unwrap();
            fs::create_dir_all(paths.states_dir()).unwrap();
            fs::write(paths.resume_state(), b"resume").unwrap();
            fs::write(paths.snapshot_state(7), b"snapshot").unwrap();
            fs::write(paths.snapshot_image(7), b"png").unwrap();
            let mut app = headless_app(dir.path()).with_library(Some(library));
            app.set_mode(Mode::Library);

            app.perform(UiAction::Remove { id, delete_saves });

            assert!(app.library.as_ref().unwrap().entries().is_empty());
            assert_eq!(sav.exists(), !delete_saves, "delete_saves = {delete_saves}");
            for file in [
                paths.resume_state(),
                paths.snapshot_state(7),
                paths.snapshot_image(7),
            ] {
                assert_eq!(file.exists(), !delete_saves, "{}", file.display());
            }
            assert_eq!(paths.states_dir().exists(), !delete_saves);
        }
    }

    #[test]
    fn quit_from_a_menu_exits() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        assert_eq!(app.perform(UiAction::Quit), Effect::Exit);
    }

    // --- resume point -----------------------------------------------------------------

    fn resume_file(dir: &Path) -> PathBuf {
        dir.join("g.states/resume.state")
    }

    /// A loader that powers on `ONE`, with its files beside `dir/one.gb`.
    fn one_loader(dir: &Path) -> Loader<GameBoy> {
        let paths = GamePaths::beside(&dir.join("one.gb"));
        Box::new(move |_| Ok((GameBoy::new(titled_rom(b"ONE")).unwrap(), paths.clone())))
    }

    /// The state of `ONE` a few frames after power-on, and the file it was left in.
    fn left_one_state(dir: &Path) -> Vec<u8> {
        let mut gb = GameBoy::new(titled_rom(b"ONE")).unwrap();
        for _ in 0..3 {
            gb.step_frame();
        }
        let left = gb.save_state();
        fs::create_dir_all(dir.join("one.states")).unwrap();
        fs::write(dir.join("one.states/resume.state"), &left).unwrap();
        left
    }

    #[test]
    fn leaving_to_the_library_writes_the_resume_point() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path()).with_resume(true);
        for _ in 0..3 {
            app.step_once();
        }
        let left = app.core.save_state();
        app.on_key(KeyCode::Escape, true);
        assert!(!resume_file(dir.path()).exists(), "not for a mere pause");
        app.perform(UiAction::BackToLibrary);
        assert_eq!(fs::read(resume_file(dir.path())).unwrap(), left);
        assert!(app.storage.is_none());
    }

    #[test]
    fn quitting_writes_the_resume_point_after_the_battery_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path()).with_resume(true);
        app.step_once();
        change_ram(&mut app, 0xAB);
        let left = app.core.save_state();
        assert_eq!(app.perform(UiAction::Quit), Effect::Exit);
        app.finish(); // what `exiting` runs for Quit, the window's close button and Ctrl-C
        assert_eq!(fs::read(resume_file(dir.path())).unwrap(), left);
        assert_eq!(fs::read(dir.path().join("g.sav")).unwrap()[0], 0xAB);
        let sav = fs::metadata(dir.path().join("g.sav"))
            .unwrap()
            .modified()
            .unwrap();
        let resume = fs::metadata(resume_file(dir.path()))
            .unwrap()
            .modified()
            .unwrap();
        assert!(
            resume >= sav,
            "so the next start does not take it for stale"
        );
    }

    #[test]
    fn the_resume_point_is_refreshed_every_minute_of_play() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path()).with_resume(true);
        let t0 = Instant::now();
        app.tick_saves(t0 + Duration::from_secs(59));
        assert!(!resume_file(dir.path()).exists(), "not before a minute");
        app.tick_saves(t0 + Duration::from_secs(61));
        assert!(resume_file(dir.path()).is_file());
        fs::remove_file(resume_file(dir.path())).unwrap();
        app.tick_saves(t0 + Duration::from_secs(100));
        assert!(
            !resume_file(dir.path()).exists(),
            "a minute from the last write"
        );
        app.tick_saves(t0 + Duration::from_secs(125));
        assert!(resume_file(dir.path()).is_file());
    }

    #[test]
    fn opening_a_game_continues_from_its_resume_point() {
        let dir = tempfile::tempdir().unwrap();
        let left = left_one_state(dir.path());
        let (library, id, _) = library_with_a_game(dir.path());
        let mut app = headless_app(dir.path())
            .with_library(Some(library))
            .with_resume(true);
        app.loader = Some(one_loader(dir.path()));
        app.set_mode(Mode::Library);

        app.perform(UiAction::Play(id));

        assert_eq!(app.mode, Mode::Playing);
        assert_eq!(app.core.save_state(), left);
        assert!(
            app.toasts
                .iter()
                .any(|t| t.text == "Resumed where you left off")
        );
    }

    #[test]
    fn a_game_given_on_the_command_line_continues_too() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = headless_app(dir.path()).with_resume(true);
        for _ in 0..4 {
            first.step_once();
        }
        let left = first.core.save_state();
        first.finish();

        let app = headless_app(dir.path())
            .with_resume(true)
            .continue_game(&[]);
        assert_eq!(app.core.save_state(), left);
        assert!(
            app.toasts
                .iter()
                .any(|t| t.text == "Resumed where you left off")
        );
        // With nothing to continue from, nothing is said.
        let other = tempfile::tempdir().unwrap();
        let app = headless_app(other.path())
            .with_resume(true)
            .continue_game(&[]);
        assert!(app.toasts.is_empty());
    }

    #[test]
    fn a_dropped_rom_leaves_the_game_it_replaces_with_a_resume_point() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path()).with_resume(true);
        app.step_once();
        let left = app.core.save_state();
        app.loader = Some(one_loader(dir.path()));
        let dropped = dir.path().join("one.gb");
        fs::write(&dropped, titled_rom(b"ONE")).unwrap();
        app.open_dropped(dropped);
        assert_eq!(fs::read(resume_file(dir.path())).unwrap(), left);
        assert_eq!(app.title, "gb - ONE");
    }

    #[test]
    fn a_recording_starts_from_power_on_and_leaves_no_resume_point() {
        let dir = tempfile::tempdir().unwrap();
        let rom = joypad_to_palette_rom();
        // A resume point from an earlier, interactive run.
        let mut earlier =
            headless_app_with(dir.path(), GameBoy::new(rom.clone()).unwrap()).with_resume(true);
        earlier.step_once();
        earlier.finish();
        let old = fs::read(resume_file(dir.path())).unwrap();

        let (app, _) = recording_app(dir.path(), &rom, Model::Dmg);
        let power_on = app.core.save_state();
        let mut app = app.with_resume(true);
        assert!(!app.resume_enabled);
        app = app.continue_game(&[]);
        assert_eq!(app.core.save_state(), power_on, "powered on, not resumed");
        assert!(app.toasts.is_empty());
        run_frames(&mut app, 3);
        app.tick_saves(Instant::now() + Duration::from_secs(120));
        app.perform(UiAction::BackToLibrary);
        app.finish();
        assert_eq!(
            fs::read(resume_file(dir.path())).unwrap(),
            old,
            "the old point is untouched and no new one written"
        );
    }

    #[test]
    fn the_resume_point_stays_off_for_a_recording_that_has_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let rom = joypad_to_palette_rom();
        let (app, _) = recording_app(dir.path(), &rom, Model::Dmg);
        let mut app = app.with_resume(true);
        run_frames(&mut app, 2);
        app.save_snapshot();
        app.load_latest_snapshot(); // stops the recording
        assert!(app.recorder.is_none());
        app.perform(UiAction::BackToLibrary);
        assert!(!resume_file(dir.path()).exists());
    }

    #[test]
    fn the_test_pattern_neither_writes_nor_uses_a_resume_point() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.test = Some(TestMode {
            frame: 0,
            fb: Box::new([0; SCREEN_WIDTH * SCREEN_HEIGHT]),
            tone: ToneGen::new(48_000, 60.0),
        });
        let mut app = app.with_resume(true);
        assert!(!app.resume_enabled);
        app.tick_saves(Instant::now() + Duration::from_secs(120));
        app.finish();
        assert!(!dir.path().join("g.states").exists());

        // And `App::new` with the flag turns it off whatever the builders say.
        let mut gb = GameBoy::new(vec![0u8; 0x8000]).unwrap();
        let saves = SaveManager::new(&dir.path().join("t.sav"), Instant::now());
        gb.set_audio_sample_rate(48_000);
        let paths = GamePaths::beside(&dir.path().join("t.gb"));
        let mut app = App::new(
            gb,
            Storage {
                saves,
                states: StateStore::new(paths.clone()),
                paths,
            },
            None,
            [[0; 4]; 4],
            1,
            true,
            ShutdownFlag::default(),
        )
        .with_resume(true);
        app.finish();
        assert!(!dir.path().join("t.states").exists());
    }

    #[test]
    fn a_corrupt_resume_point_falls_back_to_power_on_and_is_kept_as_bad() {
        let dir = tempfile::tempdir().unwrap();
        let states = dir.path().join("g.states");
        fs::create_dir_all(&states).unwrap();
        fs::write(states.join("resume.state"), b"not a state").unwrap();
        let power_on = headless_app(dir.path()).core.save_state();

        let app = headless_app(dir.path())
            .with_resume(true)
            .continue_game(&[]);

        assert_eq!(app.core.save_state(), power_on);
        assert!(!states.join("resume.state").exists());
        let bad = aside_files(&states, "bad");
        assert_eq!(bad.len(), 1);
        assert_eq!(fs::read(&bad[0]).unwrap(), b"not a state");
        let toast = app
            .toasts
            .iter()
            .find(|t| t.level == log::Level::Warn)
            .unwrap();
        assert!(toast.text.contains("Could not resume"), "{}", toast.text);
        let name = bad[0].file_name().unwrap().to_string_lossy();
        assert!(toast.text.contains(&*name), "{}", toast.text);
        // Long enough to read, and the title says it too.
        assert!(
            toast.until > Instant::now() + Duration::from_secs(6),
            "the warning stays up for more than the usual five seconds"
        );
        assert!(app.status.as_ref().unwrap().0.contains("Could not resume"));
    }

    /// The put-aside resume points of one kind (`bad` or `mismatch`) in `states`.
    fn aside_files(states: &Path, ext: &str) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = fs::read_dir(states)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.starts_with("resume-") && name.ends_with(&format!(".{ext}"))
            })
            .collect();
        found.sort();
        found
    }

    #[test]
    fn a_resume_point_from_other_options_is_kept_as_a_mismatch_and_says_how_to_get_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut cgb = GameBoy::with_model(joypad_to_cgb_palette_rom(), Model::Cgb).unwrap();
        cgb.step_frame();
        let states = dir.path().join("g.states");
        fs::create_dir_all(&states).unwrap();
        fs::write(states.join("resume.state"), cgb.save_state()).unwrap();
        let dmg = GameBoy::with_model(joypad_to_cgb_palette_rom(), Model::Dmg).unwrap();

        let app = headless_app_with(dir.path(), dmg)
            .with_resume(true)
            .continue_game(&[]);

        let kept = aside_files(&states, "mismatch");
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(fs::read(&kept[0]).unwrap(), cgb.save_state());
        assert!(
            aside_files(&states, "bad").is_empty(),
            "nothing is wrong with it"
        );
        let toast = app
            .toasts
            .iter()
            .find(|t| t.text.contains("Could not resume"))
            .unwrap();
        assert!(
            toast.text.contains("rename it to resume.state"),
            "{}",
            toast.text
        );
        assert!(toast.text.contains("--model"), "{}", toast.text);
        assert!(toast.until > Instant::now() + Duration::from_secs(6));
    }

    #[cfg(unix)]
    #[test]
    fn a_resume_point_that_cannot_be_put_aside_is_not_written_over() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let states = dir.path().join("g.states");
        fs::create_dir_all(&states).unwrap();
        fs::write(
            states.join("resume.state"),
            b"precious but unreadable to the core",
        )
        .unwrap();
        // A folder that cannot be changed: the file can be read but not moved.
        fs::set_permissions(&states, fs::Permissions::from_mode(0o555)).unwrap();
        let writable = fs::write(states.join("probe"), b"x").is_ok();
        let mut app = headless_app(dir.path())
            .with_resume(true)
            .continue_game(&[]);
        fs::set_permissions(&states, fs::Permissions::from_mode(0o755)).unwrap();
        if writable {
            return; // running as root: permissions do not apply
        }

        assert!(app.freeze_resume);
        assert!(
            app.toasts
                .iter()
                .any(|t| t.text.contains("not overwritten"))
        );
        app.step_once();
        app.tick_saves(Instant::now() + Duration::from_secs(120));
        app.perform(UiAction::BackToLibrary);
        app.finish();
        assert_eq!(
            fs::read(states.join("resume.state")).unwrap(),
            b"precious but unreadable to the core",
            "left exactly as it was"
        );
    }

    /// A power-on option on the command line: the game starts fresh, the point stays as it
    /// was through every kind of write, and a plain start afterwards still continues.
    #[test]
    fn a_power_on_option_starts_fresh_and_leaves_the_resume_point_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut earlier = headless_app(dir.path()).with_resume(true);
        for _ in 0..4 {
            earlier.step_once();
        }
        let left = earlier.core.save_state();
        earlier.finish();
        let power_on = headless_app(dir.path()).core.save_state();
        let before = fs::read(resume_file(dir.path())).unwrap();

        let mut app = headless_app(dir.path())
            .with_resume(true)
            .continue_game(&["--rtc-start"]);

        assert_eq!(app.core.save_state(), power_on, "not resumed");
        let toast = app
            .toasts
            .iter()
            .next()
            .expect("the player is told")
            .clone();
        assert_eq!(
            toast.text,
            "Started fresh because of --rtc-start; your resume point is kept (open from the \
             library to continue)"
        );
        assert!(toast.until > Instant::now() + Duration::from_secs(6));
        for _ in 0..3 {
            app.step_once();
        }
        app.tick_saves(Instant::now() + Duration::from_secs(120)); // the minute tick
        assert_eq!(
            fs::read(resume_file(dir.path())).unwrap(),
            before,
            "after the tick"
        );
        app.perform(UiAction::BackToLibrary); // leaving
        assert_eq!(
            fs::read(resume_file(dir.path())).unwrap(),
            before,
            "after leaving"
        );
        app.finish(); // quitting
        assert_eq!(
            fs::read(resume_file(dir.path())).unwrap(),
            before,
            "after quitting"
        );
        assert!(aside_files(&dir.path().join("g.states"), "bad").is_empty());

        // The next plain start continues from it, and writes again.
        let mut next = headless_app(dir.path())
            .with_resume(true)
            .continue_game(&[]);
        assert_eq!(next.core.save_state(), left);
        next.step_once();
        next.finish();
        assert_ne!(fs::read(resume_file(dir.path())).unwrap(), before);
    }

    #[test]
    fn a_power_on_option_with_no_resume_point_to_protect_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path())
            .with_resume(true)
            .continue_game(&["--model"]);
        assert!(app.toasts.is_empty(), "nothing to say");
        assert!(!app.freeze_resume);
        app.step_once();
        app.finish();
        assert!(
            resume_file(dir.path()).is_file(),
            "the session can be resumed"
        );
    }

    #[test]
    fn a_game_opened_later_in_the_session_is_not_frozen() {
        let dir = tempfile::tempdir().unwrap();
        let mut earlier = headless_app(dir.path()).with_resume(true);
        earlier.step_once();
        earlier.finish();
        let mut app = headless_app(dir.path())
            .with_resume(true)
            .continue_game(&["--model"]);
        assert!(app.freeze_resume);
        app.perform(UiAction::BackToLibrary);
        assert!(!app.freeze_resume, "the frozen game was let go of");
    }

    #[test]
    fn a_resume_point_of_another_rom_is_set_aside_when_a_library_game_opens() {
        let dir = tempfile::tempdir().unwrap();
        let other = GameBoy::new(vec![0u8; 0x8000]).unwrap().save_state();
        fs::create_dir_all(dir.path().join("one.states")).unwrap();
        fs::write(dir.path().join("one.states/resume.state"), &other).unwrap();
        let (library, id, _) = library_with_a_game(dir.path());
        let mut app = headless_app(dir.path())
            .with_library(Some(library))
            .with_resume(true);
        app.loader = Some(one_loader(dir.path()));
        app.set_mode(Mode::Library);

        app.perform(UiAction::Play(id));

        assert_eq!(app.mode, Mode::Playing, "the game starts anyway");
        assert_eq!(aside_files(&dir.path().join("one.states"), "bad").len(), 1);
        assert!(
            app.toasts
                .iter()
                .any(|t| t.text.contains("Could not resume"))
        );
    }

    #[test]
    fn start_fresh_discards_the_resume_point_and_keeps_the_battery_save() {
        let dir = tempfile::tempdir().unwrap();
        let left = left_one_state(dir.path());
        let (library, id, _) = library_with_a_game(dir.path());
        assert!(library.paths_for(&id).sav.parent().is_some());
        let mut app = headless_app(dir.path())
            .with_library(Some(library))
            .with_resume(true);
        let power_on = GameBoy::new(titled_rom(b"ONE")).unwrap().save_state();
        assert_ne!(left, power_on);
        app.loader = Some(one_loader(dir.path()));
        app.set_mode(Mode::Library);
        let sav = dir.path().join("one.sav");
        fs::write(&sav, b"the games own save").unwrap();

        app.perform(UiAction::StartFresh(id.clone()));

        assert_eq!(app.mode, Mode::Playing);
        assert_eq!(app.core.save_state(), power_on);
        assert!(!dir.path().join("one.states/resume.state").exists());
        assert!(!app.toasts.iter().any(|t| t.text.contains("Resumed")));
        assert_eq!(fs::read(&sav).unwrap(), b"the games own save");
        // The next open has nothing to continue from.
        app.perform(UiAction::BackToLibrary);
        fs::remove_file(dir.path().join("one.states/resume.state")).ok();
        app.perform(UiAction::Play(id));
        assert_eq!(app.core.save_state(), power_on);
    }

    #[test]
    fn start_fresh_on_a_missing_game_keeps_the_resume_point() {
        let dir = tempfile::tempdir().unwrap();
        let left = left_one_state(dir.path());
        let (library, id, _) = library_with_a_game(dir.path());
        fs::remove_file(&library.get(&id).unwrap().rom_path).unwrap();
        let library = Library::open(library.data_dir());
        let mut app = headless_app(dir.path()).with_library(Some(library));
        app.loader = Some(Box::new(|_| panic!("a missing game must not be loaded")));
        app.set_mode(Mode::Library);
        app.perform(UiAction::StartFresh(id));
        assert_eq!(
            fs::read(dir.path().join("one.states/resume.state")).unwrap(),
            left
        );
    }

    #[test]
    fn restart_powers_the_game_on_again_and_keeps_the_battery_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path())
            .with_resume(true)
            .with_rom_path(Some(dir.path().join("one.gb")));
        app.loader = Some(one_loader(dir.path()));
        let power_on = GameBoy::new(titled_rom(b"ONE")).unwrap().save_state();
        app.step_once();
        app.on_key(KeyCode::Escape, true);
        app.perform(UiAction::Restart);
        assert_eq!(app.mode, Mode::Playing);
        assert_eq!(app.core.save_state(), power_on);
        assert!(!dir.path().join("one.states/resume.state").exists());
        assert!(
            !resume_file(dir.path()).exists(),
            "the old point is not written"
        );
        assert_eq!(app.title, "gb - ONE");
        // No ROM to read it from: nothing happens.
        let mut app = headless_app(dir.path());
        app.on_key(KeyCode::Escape, true);
        app.perform(UiAction::Restart);
        assert_eq!(app.mode, Mode::Paused);
    }

    #[test]
    fn the_library_knows_which_games_resume() {
        let dir = tempfile::tempdir().unwrap();
        let (library, id, _) = library_with_a_game(dir.path());
        assert!(!library.has_resume(&id));
        let paths = library.paths_for(&id);
        fs::create_dir_all(paths.states_dir()).unwrap();
        fs::write(paths.resume_state(), b"x").unwrap();
        assert!(library.has_resume(&id));
    }

    // --- review fixes ---------------------------------------------------------------

    #[test]
    fn esc_quits_the_test_pattern_instead_of_pausing_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.test = Some(TestMode {
            frame: 0,
            fb: Box::new([0; SCREEN_WIDTH * SCREEN_HEIGHT]),
            tone: ToneGen::new(48_000, 60.0),
        });
        assert_eq!(app.on_key(KeyCode::Escape, true), Effect::Exit);
        assert_eq!(app.mode, Mode::Playing);
    }

    #[test]
    fn back_to_library_lets_go_of_the_game() {
        let dir = tempfile::tempdir().unwrap();
        let sav = dir.path().join("g.sav");
        let mut app = headless_app(dir.path());
        app.on_key(KeyCode::Escape, true);
        change_ram(&mut app, 0xAB); // since the pause wrote the save
        app.perform(UiAction::BackToLibrary);
        assert_eq!(fs::read(&sav).unwrap()[0], 0xAB, "written on the way out");

        assert!(app.storage.is_none());
        assert!(app.core.battery_ram().is_none(), "an idle core stands in");
        // Nothing writes for the old game any more: not the timer, not the exit flush,
        // not a screenshot.
        fs::remove_file(&sav).unwrap();
        app.tick_saves(Instant::now() + crate::save::SAVE_INTERVAL * 2);
        app.finish();
        app.screenshot();
        app.save_snapshot();
        assert!(
            fs::read_dir(dir.path()).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with('g')),
            "no file of the old game appeared"
        );
        assert!(!sav.exists());
    }

    #[test]
    fn a_menu_opened_twice_with_nothing_held_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let rom = joypad_to_palette_rom();
        let (mut app, rec_path) = recording_app(dir.path(), &rom, Model::Dmg);
        for _ in 0..2 {
            app.on_key(KeyCode::Escape, true);
            app.on_key(KeyCode::Escape, true);
        }
        assert_eq!(button_events(&mut app, &rec_path), vec![]);
    }

    #[test]
    fn losing_focus_over_a_menu_does_not_release_twice() {
        let dir = tempfile::tempdir().unwrap();
        let rom = joypad_to_palette_rom();
        let (mut app, rec_path) = recording_app(dir.path(), &rom, Model::Dmg);
        app.on_key(KeyCode::ArrowLeft, true);
        app.on_key(KeyCode::Escape, true); // released to the game here
        app.release_all_buttons(); // focus lost over the menu
        app.on_key(KeyCode::Escape, true);
        assert_eq!(
            button_events(&mut app, &rec_path),
            vec![(Button::Left, true), (Button::Left, false)]
        );
    }

    #[test]
    fn the_window_is_resized_only_when_the_picture_size_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.scale = 3;
        // Started on the library, the window has its size: the first game fits it.
        assert_eq!(app.fit_request(), Some((480, 432)));
        // Another game of the same size, or a trip through the library, leaves it alone.
        assert_eq!(app.fit_request(), None);
        app.perform(UiAction::BackToLibrary);
        assert_eq!(app.fit_request(), None);
        // A different picture size does resize it.
        app.fitted = Some((240, 160));
        assert_eq!(app.fit_request(), Some((480, 432)));
    }

    #[test]
    fn a_stale_animation_wake_is_dropped_when_the_screen_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.loader = Some(Box::new(|path| {
            Ok((
                GameBoy::new(titled_rom(b"OTHER")).unwrap(),
                GamePaths::beside(path),
            ))
        }));
        let soon = Some(Instant::now());
        app.ui_wake = soon;
        app.toggle_pause();
        assert!(app.ui_wake.is_none(), "pause");
        app.ui_wake = soon;
        app.toggle_pause();
        assert!(app.ui_wake.is_none(), "resume");
        app.ui_wake = soon;
        app.open_dropped(dir.path().join("other.gb"));
        assert!(app.ui_wake.is_none(), "open");
    }
}
