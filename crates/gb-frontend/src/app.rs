//! The winit application: window, input, frame pacing and presentation.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use emu_common::replay::{Event, Record};
use emu_common::{Button, Core, FrameRef, Pixels as FramePixels, System};
use gb_core::{Framebuffer, SCREEN_HEIGHT, SCREEN_WIDTH};
use pixels::{Pixels, SurfaceTexture};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

use crate::audio::{AudioOutput, ToneGen};
use crate::keymap::{Action, InputState, map};
use crate::pacer::{FramePacer, Steps, audio_seconds, run_due};
use crate::record::{self, Recorder};
use crate::save::SaveManager;
use crate::shutdown::ShutdownFlag;
use crate::state::StateSlots;
use crate::video::{Palette, present, test_pattern};

/// Speed multiplier while Tab is held.
const FAST_FORWARD_SPEED: f64 = 4.0;

/// How long a save-state message stays in the window title.
const STATUS_DURATION: Duration = Duration::from_secs(2);

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

/// How the console is named in a message to the user.
fn system_label(system: System) -> &'static str {
    match system {
        System::Gb => "Game Boy",
        System::Gba => "GBA",
    }
}

/// State for `--test-pattern`: the core is bypassed.
struct TestMode {
    frame: u32,
    fb: Box<Framebuffer>,
    tone: ToneGen,
}

/// Where progress is persisted: battery RAM and save-state slots.
pub struct Storage {
    pub saves: SaveManager,
    pub states: StateSlots,
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
    saves: SaveManager,
    states: StateSlots,
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
    /// Save-state feedback shown in the title until the given instant.
    status: Option<(String, Instant)>,
    test: Option<TestMode>,
    shutdown: ShutdownFlag,
    rom_path: PathBuf,
    recorder: Option<Recorder>,
    /// Emulated frames since power-on (the clock recordings are keyed by).
    frame: u64,

    input: InputState,
    pacer: FramePacer,
    last_tick: Instant,
    paused: bool,
    fast_forward: bool,
    /// False while the window has a zero-sized surface (minimized).
    surface_ok: bool,

    window: Option<Arc<Window>>,
    pixels: Option<Pixels<'static>>,
    /// Set when the app must stop with a failure; reported by `main`.
    pub error: Option<String>,
}

impl<C: Core> App<C> {
    pub fn new(
        core: C,
        Storage { saves, states }: Storage,
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
            saves,
            states,
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
            rom_path: PathBuf::from("screenshot.gb"),
            recorder: None,
            frame: 0,
            input: InputState::default(),
            pacer: FramePacer::new(fps),
            last_tick: Instant::now(),
            paused: false,
            fast_forward: false,
            surface_ok: true,
            window: None,
            pixels: None,
            error: None,
        }
    }

    /// Screenshots are saved next to this ROM path.
    pub fn with_rom_path(mut self, rom_path: PathBuf) -> Self {
        self.rom_path = rom_path;
        self
    }

    /// Record every input from power-on (see `record.rs`).
    pub fn with_recorder(mut self, recorder: Option<Recorder>) -> Self {
        self.recorder = recorder;
        self
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, msg: String) {
        self.error = Some(msg);
        event_loop.exit();
    }

    /// Exit the loop (and so flush saves) if a termination signal was received.
    fn check_shutdown(&self, event_loop: &ActiveEventLoop) -> bool {
        let requested = self.shutdown.requested();
        if requested && !event_loop.exiting() {
            eprintln!("received a termination signal; saving and exiting");
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
            let paused = if self.paused { " [paused]" } else { "" };
            let status = self
                .status
                .as_ref()
                .map_or(String::new(), |(msg, _)| format!(" - {msg}"));
            w.set_title(&format!("{}{rec}{paused}{status}", self.title));
        }
    }

    /// Report a save-state result on stderr and, briefly, in the title.
    fn show_status(&mut self, msg: String) {
        eprintln!("{msg}");
        self.status = Some((msg, Instant::now() + STATUS_DURATION));
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
            eprintln!("recording stopped: {why}, so the rest could not be replayed");
            r.finish(self.frame);
            self.update_title();
        }
    }

    /// Say so, instead of touching a slot file, if the core has no state format
    /// (the GBA until P3). True when states are available.
    fn states_available(&mut self) -> bool {
        if self.core.supports_save_state() {
            return true;
        }
        let label = system_label(self.core.system());
        self.show_status(format!("save states are not supported for {label} yet"));
        false
    }

    fn save_state(&mut self) {
        if !self.states_available() {
            return;
        }
        let msg = self.states.save(&self.core).unwrap_or_else(|e| e);
        self.show_status(msg);
    }

    fn load_state(&mut self) {
        if !self.states_available() {
            return;
        }
        match self.states.load(&mut self.core) {
            Ok(msg) => {
                // The state replaces the machine (and the frame it is on), so
                // the replay of a recording that goes on would diverge.
                self.stop_recording("a save state was loaded");
                // The state holds the buttons pressed when it was saved; apply
                // the keys held now instead.
                for button in Button::ALL {
                    self.set_button(button, self.input.is_down(button));
                }
                self.clear_audio();
                self.reset_clock();
                // Show the restored picture even while paused.
                self.upload();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                self.show_status(msg);
            }
            Err(msg) => self.show_status(msg),
        }
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if self.paused {
            // The periodic save tick doesn't run while paused; persist now.
            self.saves.flush(&self.core);
        }
        self.clear_audio();
        self.reset_clock();
        self.update_title();
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
        if let Some(r) = self.recorder.as_mut() {
            r.record(Record {
                frame: self.frame,
                event: Event::Button { button, pressed },
            });
        }
    }

    fn on_key(&mut self, event_loop: &ActiveEventLoop, key: winit::keyboard::KeyCode, down: bool) {
        self.on_key_input(key, down);
        match map(key) {
            Some(Action::Quit) if down => event_loop.exit(),
            Some(Action::Pause) if down => self.toggle_pause(),
            Some(Action::FastForward) => self.set_fast_forward(down),
            // States need the core, which `--test-pattern` bypasses.
            _ if !down || self.test.is_some() => {}
            Some(Action::SaveState) => self.save_state(),
            Some(Action::LoadState) => self.load_state(),
            Some(Action::SelectSlot(slot)) => {
                self.states.select(slot);
                self.show_status(format!("state slot {}", self.states.slot()));
            }
            _ => {}
        }
    }

    /// The part of key handling that needs no event loop (joypad, screenshot).
    fn on_key_input(&mut self, key: winit::keyboard::KeyCode, down: bool) {
        if let Some((button, pressed)) = self.input.key(key, down) {
            self.set_button(button, pressed);
        }
        if down && map(key) == Some(Action::Screenshot) {
            self.screenshot();
        }
    }

    fn release_all_buttons(&mut self) {
        for button in self.input.release_all() {
            self.set_button(button, false);
        }
    }

    /// Save the picture on screen next to the ROM and mark the frame.
    fn screenshot(&mut self) {
        let path = record::screenshot_path(&self.rom_path, self.frame);
        let frame = frame_of(self.test.as_ref(), &self.core);
        match record::write_png(frame, &path) {
            Ok(()) => eprintln!("screenshot: {} (frame {})", path.display(), self.frame),
            Err(e) => eprintln!("warning: cannot write screenshot {}: {e}", path.display()),
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
            eprintln!("warning: cannot resize surface to {width}x{height}: {e}");
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
                // Always drain so the core's buffer cannot grow unbounded.
                self.core.drain_audio_into(&mut self.audio_buf);
                let seconds = audio_seconds(self.audio_buf.len(), self.core.audio_sample_rate());
                // A step that made no audio (a core whose APU produces none yet)
                // still covered a frame: without this the pacer would see zero
                // time pass and run flat out. A Game Boy step always has samples.
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
                    eprintln!(
                        "warning: cannot resize the frame buffer to {}x{}: {e}; the picture stops updating",
                        size.0, size.1
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

impl<C: Core> ApplicationHandler for App<C> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // The window and the pixel buffer follow the first frame's size.
        let first = frame_of(self.test.as_ref(), &self.core);
        let (width, height) = (first.width as u32, first.height as u32);
        self.buffer_size = (width, height);
        let attrs = Window::default_attributes()
            .with_title(self.title.as_str())
            .with_inner_size(LogicalSize::new(width * self.scale, height * self.scale))
            .with_min_inner_size(LogicalSize::new(width, height));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => return self.fail(event_loop, format!("cannot create window: {e}")),
        };
        // Physical pixels: on HiDPI displays this is `scale * logical size`.
        let size = window.inner_size();
        let surface =
            SurfaceTexture::new(size.width.max(1), size.height.max(1), Arc::clone(&window));
        let mut pixels = match Pixels::new(width, height, surface) {
            Ok(p) => p,
            Err(e) => return self.fail(event_loop, format!("cannot initialise graphics: {e}")),
        };
        pixels.clear_color(pixels::wgpu::Color::BLACK);
        // The window may already be minimised (zero-sized) when it is created.
        self.surface_ok = surface_extent(size.width, size.height).is_some();
        self.window = Some(window);
        self.pixels = Some(pixels);
        self.update_title();
        self.upload();
        self.reset_clock();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
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
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
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
            WindowEvent::KeyboardInput { event, .. } if !event.repeat => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    self.on_key(event_loop, code, event.state == ElementState::Pressed);
                }
            }
            WindowEvent::RedrawRequested => {
                if !self.surface_ok {
                    return;
                }
                if let Some(p) = &self.pixels
                    && let Err(e) = p.render()
                {
                    self.fail(event_loop, format!("render failed: {e}"));
                }
            }
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

        if self.paused {
            // Wake once more to clear a status message from the title.
            event_loop
                .set_control_flow(status_until.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
            return;
        }
        self.pacer.advance(dt);
        if self.run_due()
            && self.surface_ok
            && let Some(w) = &self.window
        {
            w.request_redraw();
        }
        self.saves.tick(&self.core, now);

        // Never sleep past the next frame deadline (saves are checked once per wake).
        let wait = self.pacer.time_until_next().max(Duration::from_micros(200));
        event_loop.set_control_flow(ControlFlow::WaitUntil(now + wait));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.saves.flush_final(&self.core);
        if let Some(r) = self.recorder.as_mut() {
            r.finish(self.frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let mut saves = SaveManager::new(&dir.join("g.gb"), Instant::now());
        saves.load(&mut gb); // no .sav yet: the blank RAM is the baseline
        App::new(
            gb,
            Storage {
                saves,
                states: StateSlots::new(&dir.join("g.gb")),
            },
            None,
            [[0; 4]; 4],
            1,
            false,
            ShutdownFlag::default(),
        )
        .with_rom_path(dir.join("g.gb"))
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
        let mut saves = SaveManager::new(&rom_path, Instant::now());
        saves.load(&mut gb);
        let rec_path = dir.join("s.gbinput");
        let header = record::header_for(&gb, rom_hash(rom));
        assert_eq!(header.variant, u8::from(model == Model::Cgb));
        let recorder = Recorder::create(&rec_path, &header).unwrap();
        let app = App::new(
            gb,
            Storage {
                saves,
                states: StateSlots::new(&rom_path),
            },
            None,
            [[0; 4]; 4],
            1,
            false,
            ShutdownFlag::default(),
        )
        .with_rom_path(rom_path)
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
        let shot = fs::read(record::screenshot_path(&dir.join("g.gb"), marker)).unwrap();
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
    fn loading_a_state_stops_the_recording_and_resyncs_through_set_button() {
        use gb_core::replay::{Event, Recording};
        use winit::keyboard::KeyCode;

        let dir = tempfile::tempdir().unwrap();
        let rom = joypad_to_palette_rom();
        let (mut app, rec_path) = recording_app(dir.path(), &rom, Model::Dmg);
        run_frames(&mut app, 2);
        app.on_key_input(KeyCode::ArrowRight, true);
        run_frames(&mut app, 1);
        app.save_state();
        app.on_key_input(KeyCode::ArrowRight, false);
        run_frames(&mut app, 3);
        assert!(app.recorder.is_some(), "saving a state keeps recording");

        // A failed load (empty slot) changes nothing and keeps recording.
        app.states.select(2);
        app.load_state();
        assert!(app.recorder.is_some());
        app.states.select(1);

        let frame_at_load = app.frame;
        app.load_state();
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
        let mut saves = SaveManager::new(&dir.join("g.gba"), Instant::now());
        saves.load(&mut core);
        App::new(
            core,
            Storage {
                saves,
                states: StateSlots::new(&dir.join("g.gba")),
            },
            None,
            [[0; 4]; 4],
            1,
            false,
            ShutdownFlag::default(),
        )
        .with_rom_path(dir.join("g.gba"))
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

    /// K and L on a GBA say so and touch nothing: no slot file, the recording
    /// goes on, and the answer comes through `Box<dyn Core>` (a defaulted trait
    /// method that was not forwarded would say "supported").
    #[test]
    fn save_state_keys_on_a_gba_say_so_and_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let core: Box<dyn Core> = Box::new(Gba::new(gba_rom(), None).unwrap());
        let mut app = gba_app(dir.path(), core);
        app.step_once();
        app.save_state();
        assert_eq!(
            app.status.as_ref().unwrap().0,
            "save states are not supported for GBA yet"
        );
        app.status = None;
        app.load_state();
        assert!(
            app.status
                .as_ref()
                .unwrap()
                .0
                .contains("not supported for GBA")
        );
        for n in 1..=9 {
            assert!(!dir.path().join(format!("g.ss{n}")).exists());
        }
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
        let mut saves = SaveManager::new(&rom_path, Instant::now());
        saves.load(&mut gba);
        let rec_path = dir.path().join("s.gbinput");
        let header = record::header_for(&gba, rom_hash(&rom));
        assert_eq!(header.system, System::Gba);
        assert_eq!(header.battery.as_ref().map(Vec::len), Some(128 * 1024));
        let mut app = App::new(
            gba,
            Storage {
                saves,
                states: StateSlots::new(&rom_path),
            },
            None,
            [[0; 4]; 4],
            1,
            false,
            ShutdownFlag::default(),
        )
        .with_rom_path(rom_path)
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
    fn save_and_load_state_keys_restore_the_machine() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.core.step_frame();
        let snap = app.core.save_state();
        app.save_state();
        assert!(dir.path().join("g.ss1").is_file());
        assert!(app.status.as_ref().unwrap().0.contains("saved state 1"));

        for _ in 0..5 {
            app.step_once();
        }
        app.load_state();
        assert_eq!(app.core.save_state(), snap);
        assert!(app.status.as_ref().unwrap().0.contains("loaded state 1"));
    }

    #[test]
    fn failed_state_load_keeps_the_machine() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.core.step_frame();
        let before = app.core.save_state();
        app.load_state(); // slot 1 doesn't exist
        assert_eq!(app.core.save_state(), before);
        assert!(app.status.as_ref().unwrap().0.contains("empty"));
    }

    #[test]
    fn loading_a_state_rewinds_the_battery_save_after_backing_it_up() {
        let dir = tempfile::tempdir().unwrap();
        let sav = dir.path().join("g.sav");
        let mut app = headless_app(dir.path());
        app.save_state(); // blank RAM
        change_ram(&mut app, 0xAB);
        app.toggle_pause(); // writes 0xAB to g.sav
        app.load_state();
        app.toggle_pause();
        app.toggle_pause(); // pause flushes the restored RAM
        assert_eq!(fs::read(&sav).unwrap()[0], 0x00, "rewound with the state");
        assert_eq!(fs::read(dir.path().join("g.sav.bak")).unwrap()[0], 0xAB);
    }

    #[test]
    fn status_message_expires() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.show_status("hi".into());
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
}
