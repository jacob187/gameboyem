//! The winit application: window, input, frame pacing and presentation.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gb_core::{FRAMES_PER_SECOND, Framebuffer, GameBoy, SCREEN_HEIGHT, SCREEN_WIDTH};
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
use crate::save::SaveManager;
use crate::video::{Palette, blit, test_pattern};

/// Speed multiplier while Tab is held.
const FAST_FORWARD_SPEED: f64 = 4.0;

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
    palette: Palette,
    scale: u32,
    title: String,
    test: Option<TestMode>,

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
            palette,
            scale,
            title,
            test,
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
            w.set_title(&format!("{}{suffix}", self.title));
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
            _ => {}
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.surface_ok = width > 0 && height > 0;
        if !self.surface_ok {
            return;
        }
        if let Some(p) = self.pixels.as_mut()
            && let Err(e) = p.resize_surface(width, height)
        {
            eprintln!("warning: cannot resize surface to {width}x{height}: {e}");
        }
    }

    /// Run `n` emulated frames, feed audio, and upload the last picture.
    fn run_frames(&mut self, n: u32) {
        for _ in 0..n {
            let samples = match self.test.as_mut() {
                Some(t) => {
                    test_pattern(t.frame, &mut t.fb);
                    t.frame = t.frame.wrapping_add(1);
                    t.tone.next_frame()
                }
                None => {
                    self.gb.step_frame();
                    // Always drain so the core's buffer cannot grow unbounded.
                    self.gb.audio_samples()
                }
            };
            if !self.fast_forward
                && let Some(a) = &self.audio
            {
                a.ring.push(&samples);
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

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.pixels.is_none() {
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
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        self.saves.tick(&self.gb, now);

        // Never sleep past the next frame deadline (saves are checked once per wake).
        let wait = self.pacer.time_until_next().max(Duration::from_micros(200));
        event_loop.set_control_flow(ControlFlow::WaitUntil(now + wait));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.saves.flush(&self.gb);
    }
}
