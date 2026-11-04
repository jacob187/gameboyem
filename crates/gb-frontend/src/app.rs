//! The winit application: window, input, frame pacing and presentation.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gb_core::replay::{Event, Record};
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
use crate::pacer::FramePacer;
use crate::record::{self, Recorder};
use crate::save::SaveManager;
use crate::shutdown::ShutdownFlag;
use crate::video::{Palette, blit, test_pattern};

/// Speed multiplier while Tab is held.
const FAST_FORWARD_SPEED: f64 = 4.0;

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

pub struct App {
    gb: GameBoy,
    saves: SaveManager,
    audio: Option<AudioOutput>,
    /// Reused every frame to carry samples from the core to the ring.
    audio_buf: Vec<f32>,
    palette: Palette,
    scale: u32,
    title: String,
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

impl App {
    pub fn new(
        gb: GameBoy,
        saves: SaveManager,
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
            audio,
            audio_buf: Vec::new(),
            palette,
            scale,
            title,
            test,
            shutdown,
            rom_path: PathBuf::from("screenshot.gb"),
            recorder: None,
            frame: 0,
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
            w.set_title(&format!("{}{rec}{paused}", self.title));
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

    /// Every joypad change goes through here so recordings see all of them.
    fn set_button(&mut self, button: Button, pressed: bool) {
        self.gb.set_button(button, pressed);
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
        let fb: &Framebuffer = match &self.test {
            Some(t) => &t.fb,
            None => self.gb.framebuffer(),
        };
        match record::write_png(fb, &path) {
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

    /// Run `n` emulated frames, feed audio, and upload the last picture.
    fn run_frames(&mut self, n: u32) {
        for _ in 0..n {
            self.audio_buf.clear();
            match self.test.as_mut() {
                Some(t) => {
                    test_pattern(t.frame, &mut t.fb);
                    t.frame = t.frame.wrapping_add(1);
                    self.audio_buf.extend_from_slice(&t.tone.next_frame());
                }
                None => {
                    self.gb.step_frame();
                    self.frame += 1;
                    // Always drain so the core's buffer cannot grow unbounded.
                    self.gb.drain_audio_into(&mut self.audio_buf);
                }
            }
            if !self.fast_forward
                && let Some(a) = &self.audio
            {
                a.ring.push(&self.audio_buf);
            }
        }
        if n > 0 {
            self.upload();
        }
    }

    fn upload(&mut self) {
        let fb: &Framebuffer = match &self.test {
            Some(t) => &t.fb,
            None => self.gb.framebuffer(),
        };
        if let Some(p) = self.pixels.as_mut() {
            blit(fb, &self.palette, p.frame_mut());
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

        if self.paused {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        let n = self.pacer.advance(dt);
        if n > 0 {
            self.run_frames(n);
            if self.surface_ok
                && let Some(w) = &self.window
            {
                w.request_redraw();
            }
        }
        self.saves.tick(&self.gb, now);

        // Never sleep past the next frame deadline (saves are checked once per wake).
        let wait = self.pacer.time_until_next().max(Duration::from_micros(200));
        event_loop.set_control_flow(ControlFlow::WaitUntil(now + wait));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.saves.flush_final(&self.gb);
        if let Some(r) = self.recorder.as_mut() {
            r.finish(self.frame);
        }
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
            saves,
            None,
            [[0; 4]; 4],
            1,
            false,
            ShutdownFlag::default(),
        )
        .with_rom_path(dir.join("g.gb"))
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

    /// The whole loop the user relies on: play with `--record`, press the
    /// screenshot key, then replay headlessly and get the identical picture
    /// and machine state. Covers a press+release inside one frame, the
    /// focus-loss release, a short-footer RTC save, and a different audio rate.
    #[test]
    fn recording_replays_to_the_identical_state_and_screenshot() {
        use gb_core::replay::{Header, Recording, Replayer, rom_hash};
        use winit::keyboard::KeyCode;

        let dir = tempfile::tempdir().unwrap();
        let rom = joypad_to_palette_rom();
        let rom_path = dir.path().join("g.gb");
        // 32 KiB RAM + the 44-byte RTC footer variant, with recognisable content.
        let mut sav: Vec<u8> = (0..0x8000u32).map(|i| (i * 7) as u8).collect();
        sav.extend_from_slice(&[0; 44]);
        sav[0x8000] = 12; // RTC seconds
        fs::write(dir.path().join("g.sav"), &sav).unwrap();

        let mut gb = GameBoy::new(rom.clone()).unwrap();
        gb.set_audio_sample_rate(48_000);
        let mut saves = SaveManager::new(&rom_path, Instant::now());
        saves.load(&mut gb);
        assert_eq!(
            gb.battery_ram().unwrap()[..0x8000],
            sav[..0x8000],
            "save loaded"
        );
        let rec_path = dir.path().join("s.gbinput");
        let header = Header {
            rom_hash: rom_hash(&rom),
            battery: gb.battery_ram(),
        };
        let recorder = Recorder::create(&rec_path, &header).unwrap();
        let mut app = App::new(
            gb,
            saves,
            None,
            [[0; 4]; 4],
            1,
            false,
            ShutdownFlag::default(),
        )
        .with_rom_path(rom_path)
        .with_recorder(Some(recorder));

        app.run_frames(2);
        app.on_key_input(KeyCode::ArrowRight, true);
        app.run_frames(3);
        app.on_key_input(KeyCode::ArrowLeft, true); // press and release in one frame
        app.on_key_input(KeyCode::ArrowLeft, false);
        app.run_frames(2);
        app.on_key_input(KeyCode::ArrowLeft, true);
        app.run_frames(1);
        app.release_all_buttons(); // window lost focus with Right and Left held
        app.on_key_input(KeyCode::ArrowRight, true);
        app.run_frames(1);
        app.on_key_input(KeyCode::KeyC, true); // screenshot + marker
        let marker = app.frame;
        app.run_frames(4);
        let total = app.frame;
        app.recorder.as_mut().unwrap().finish(total);

        let rec = Recording::decode(&fs::read(&rec_path).unwrap()).unwrap();
        assert_eq!(rec.end_frame, Some(total));
        assert_eq!(rec.markers(), vec![marker]);
        assert_eq!(
            rec.records.len(),
            8,
            "7 button changes + 1 marker: {:?}",
            rec.records
        );

        let mut gb = rec.power_on(rom).unwrap();
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
        let shot = fs::read(record::screenshot_path(&dir.path().join("g.gb"), marker)).unwrap();
        let replayed = dir.path().join("replayed.png");
        record::write_png(gb.framebuffer(), &replayed).unwrap();
        assert_eq!(
            fs::read(&replayed).unwrap(),
            shot,
            "screenshot == replayed frame"
        );
        assert_eq!(
            gb.framebuffer()[0],
            2,
            "Right held: shade follows the joypad"
        );

        step(&mut gb, &mut player, total);
        assert_eq!(gb.mcycles(), app.gb.mcycles());
        assert_eq!(gb.cpu_registers(), app.gb.cpu_registers());
        assert_eq!(
            gb.peek(0xFF0F),
            app.gb.peek(0xFF0F),
            "IF (joypad IRQ) matches"
        );
        assert_eq!(gb.framebuffer()[..], app.gb.framebuffer()[..]);
        assert_eq!(
            gb.battery_ram(),
            app.gb.battery_ram(),
            "cart RAM + RTC match"
        );
    }

    #[test]
    fn pausing_without_a_ram_change_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.toggle_pause();
        assert!(!dir.path().join("g.sav").exists());
    }
}
