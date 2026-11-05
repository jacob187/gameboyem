//! The winit application: window, input, frame pacing and presentation.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gb_core::{Button, FRAMES_PER_SECOND, Framebuffer, GameBoy, SCREEN_HEIGHT, SCREEN_WIDTH};
use pixels::{Pixels, SurfaceTexture};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

use crate::audio::{AudioOutput, ToneGen};
use crate::keymap::{Action, InputState, map};
use crate::pacer::{FramePacer, MAX_STEPS_PER_ADVANCE, audio_seconds};
use crate::save::SaveManager;
use crate::shutdown::ShutdownFlag;
use crate::state::StateSlots;
use crate::video::{Palette, blit, blit_cgb, test_pattern};

/// Speed multiplier while Tab is held.
const FAST_FORWARD_SPEED: f64 = 4.0;

/// How long a save-state message stays in the window title.
const STATUS_DURATION: Duration = Duration::from_secs(2);

/// The size to configure the surface with, or `None` while the window has no
/// drawable area (minimised: a zero dimension makes wgpu panic or error).
fn surface_extent(width: u32, height: u32) -> Option<(u32, u32)> {
    (width > 0 && height > 0).then_some((width, height))
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

pub struct App {
    gb: GameBoy,
    saves: SaveManager,
    states: StateSlots,
    audio: Option<AudioOutput>,
    /// Reused every frame to carry samples from the core to the ring.
    audio_buf: Vec<f32>,
    palette: Palette,
    scale: u32,
    title: String,
    /// Save-state feedback shown in the title until the given instant.
    status: Option<(String, Instant)>,
    test: Option<TestMode>,
    shutdown: ShutdownFlag,

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

impl App {
    pub fn new(
        gb: GameBoy,
        Storage { saves, states }: Storage,
        audio: Option<AudioOutput>,
        palette: Palette,
        scale: u32,
        test_pattern: bool,
        shutdown: ShutdownFlag,
    ) -> Self {
        let name = gb.cartridge_header().title.trim().to_owned();
        let title = if name.is_empty() {
            "gb".to_owned()
        } else {
            format!("gb - {name}")
        };
        let test = test_pattern.then(|| TestMode {
            frame: 0,
            fb: Box::new([0; SCREEN_WIDTH * SCREEN_HEIGHT]),
            tone: ToneGen::new(
                audio
                    .as_ref()
                    .map_or(gb.audio_sample_rate(), |a| a.sample_rate),
                FRAMES_PER_SECOND,
            ),
        });
        Self {
            gb,
            saves,
            states,
            audio,
            audio_buf: Vec::new(),
            palette,
            scale,
            title,
            status: None,
            test,
            shutdown,
            input: InputState::default(),
            pacer: FramePacer::new(FRAMES_PER_SECOND),
            last_tick: Instant::now(),
            paused: false,
            fast_forward: false,
            surface_ok: true,
            window: None,
            pixels: None,
            error: None,
        }
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
            let suffix = if self.paused { " [paused]" } else { "" };
            let status = self
                .status
                .as_ref()
                .map_or(String::new(), |(msg, _)| format!(" - {msg}"));
            w.set_title(&format!("{}{suffix}{status}", self.title));
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

    fn save_state(&mut self) {
        let msg = self.states.save(&self.gb).unwrap_or_else(|e| e);
        self.show_status(msg);
    }

    fn load_state(&mut self) {
        match self.states.load(&mut self.gb) {
            Ok(msg) => {
                // The state holds the buttons pressed when it was saved; apply
                // the keys held now instead.
                for button in Button::ALL {
                    self.gb.set_button(button, self.input.is_down(button));
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
            self.saves.flush(&self.gb);
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

    fn on_key(&mut self, event_loop: &ActiveEventLoop, key: winit::keyboard::KeyCode, down: bool) {
        if let Some((button, pressed)) = self.input.key(key, down) {
            self.gb.set_button(button, pressed);
        }
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
                self.gb.step_frame();
                // Always drain so the core's buffer cannot grow unbounded.
                self.gb.drain_audio_into(&mut self.audio_buf);
                audio_seconds(self.audio_buf.len(), self.gb.audio_sample_rate())
            }
        };
        if !self.fast_forward
            && let Some(a) = &self.audio
        {
            a.ring.push(&self.audio_buf);
        }
        emulated
    }

    /// Run steps while the pacer says a frame is owed, then upload the last
    /// picture. Returns whether any step ran.
    fn run_due(&mut self) -> bool {
        let mut ran = 0;
        while ran < MAX_STEPS_PER_ADVANCE && self.pacer.frame_due() {
            let emulated = self.step_once();
            self.pacer.consume(emulated);
            ran += 1;
        }
        if ran > 0 {
            self.upload();
        }
        ran > 0
    }

    fn upload(&mut self) {
        let Some(p) = self.pixels.as_mut() else {
            return;
        };
        match &self.test {
            Some(t) => blit(&t.fb, &self.palette, p.frame_mut()),
            None => match self.gb.cgb_framebuffer() {
                Some(fb) => blit_cgb(fb, p.frame_mut()),
                None => blit(self.gb.framebuffer(), &self.palette, p.frame_mut()),
            },
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(self.title.as_str())
            .with_inner_size(LogicalSize::new(
                SCREEN_WIDTH as u32 * self.scale,
                SCREEN_HEIGHT as u32 * self.scale,
            ))
            .with_min_inner_size(LogicalSize::new(SCREEN_WIDTH as u32, SCREEN_HEIGHT as u32));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => return self.fail(event_loop, format!("cannot create window: {e}")),
        };
        // Physical pixels: on HiDPI displays this is `scale * logical size`.
        let size = window.inner_size();
        let surface =
            SurfaceTexture::new(size.width.max(1), size.height.max(1), Arc::clone(&window));
        let mut pixels = match Pixels::new(SCREEN_WIDTH as u32, SCREEN_HEIGHT as u32, surface) {
            Ok(p) => p,
            Err(e) => return self.fail(event_loop, format!("cannot initialise graphics: {e}")),
        };
        pixels.clear_color(pixels::wgpu::Color::BLACK);
        // The window may already be minimised (zero-sized) when it is created.
        self.surface_ok = surface_extent(size.width, size.height).is_some();
        self.window = Some(window);
        self.pixels = Some(pixels);
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
                    for button in self.input.release_all() {
                        self.gb.set_button(button, false);
                    }
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
        self.saves.tick(&self.gb, now);

        // Never sleep past the next frame deadline (saves are checked once per wake).
        let wait = self.pacer.time_until_next().max(Duration::from_micros(200));
        event_loop.set_control_flow(ControlFlow::WaitUntil(now + wait));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.saves.flush_final(&self.gb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
    fn headless_app(dir: &std::path::Path) -> App {
        let mut rom = vec![0u8; 0x8000];
        rom[0x147] = 0x03; // MBC1+RAM+BATTERY
        rom[0x149] = 0x02; // 8 KiB
        let mut gb = GameBoy::new(rom).unwrap();
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
    }

    fn change_ram(app: &mut App, first_byte: u8) {
        let mut ram = app.gb.battery_ram().unwrap();
        ram[0] = first_byte;
        app.gb.load_battery_ram(&ram).unwrap();
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

    #[test]
    fn a_normal_step_covers_one_frame_of_emulated_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        let period = app.pacer.period();
        app.step_once(); // the first frame starts mid-frame after the boot state
        let total: f64 = (0..120).map(|_| app.step_once()).sum();
        // Samples are whole, so the total is within a sample of 120 periods.
        let rate = f64::from(app.gb.audio_sample_rate());
        assert!((total - 120.0 * period).abs() < 2.0 / rate, "{total}");
    }

    #[test]
    fn save_and_load_state_keys_restore_the_machine() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.gb.step_frame();
        let snap = app.gb.save_state();
        app.save_state();
        assert!(dir.path().join("g.ss1").is_file());
        assert!(app.status.as_ref().unwrap().0.contains("saved state 1"));

        for _ in 0..5 {
            app.step_once();
        }
        app.load_state();
        assert_eq!(app.gb.save_state(), snap);
        assert!(app.status.as_ref().unwrap().0.contains("loaded state 1"));
    }

    #[test]
    fn failed_state_load_keeps_the_machine() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.gb.step_frame();
        let before = app.gb.save_state();
        app.load_state(); // slot 1 doesn't exist
        assert_eq!(app.gb.save_state(), before);
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
