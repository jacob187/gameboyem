//! The screens, drawn with egui into the window the game is shown in: the game list,
//! the pause menu and the help. egui draws over (or, in the library, instead of) the
//! picture, and only while a screen or a toast is up; see `App` for when.
//!
//! Drawing never does the work itself. A click is collected as a [`UiAction`] and the
//! app carries it out after the frame, so a file dialog or a file write never runs in
//! the middle of drawing.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::Path;
use std::time::Duration;

use egui::{
    Align, Align2, Color32, FontId, Id, Layout, Rect, RichText, Sense, TextureHandle,
    TextureOptions, pos2, vec2,
};
use pixels::Pixels;
use pixels::wgpu;
use winit::event::WindowEvent;
use winit::window::Window;

use crate::keymap;
use crate::library::{Entry, GameId, Library, Source};
use crate::save::read_capped;
use crate::state::MAX_IMAGE_BYTES;
use crate::ui::{Mode, SnapshotItem, Toasts, UiAction, friendly_time_local, library_order};

/// Largest thumbnail side drawn; the library refuses larger ones when they are set, and
/// a file placed in the folder by hand is held to the same limit.
const MAX_THUMBNAIL_SIDE: u32 = 1024;

const CARD_WIDTH: f32 = 176.0;
const CARD_HEIGHT: f32 = 188.0;
const CARD_PAD: f32 = 8.0;
const THUMB_HEIGHT: f32 = 128.0;
const CARD_GAP: f32 = 12.0;

/// A row of the snapshot list: the picture's size (a Game Boy frame at half size) and the row.
const SNAP_WIDTH: f32 = 80.0;
const SNAP_HEIGHT: f32 = 72.0;

const ACCENT: Color32 = Color32::from_rgb(0x3b, 0x82, 0xc4);
const BADGE: Color32 = Color32::from_rgb(0x48, 0x4f, 0x5c);
const MISSING: Color32 = Color32::from_rgb(0xd0, 0x5a, 0x4e);

/// What the screens show: a snapshot taken from the app for one frame.
pub struct Screen<'a> {
    pub mode: Mode,
    /// The game list; `None` when there is no data folder to keep one in.
    pub library: Option<&'a Library>,
    /// The resolved app data folder, `None` when it could not be used.
    pub data_dir: Option<&'a Path>,
    /// The title of the running game (for the pause menu).
    pub game_title: &'a str,
    /// The running game has a real core: snapshots and screenshots mean something.
    pub can_save: bool,
    /// The game can be powered on again (there is a ROM to read it from).
    pub can_restart: bool,
    /// The running game's snapshots, newest first.
    pub snapshots: &'a [SnapshotItem],
    /// The library games that have a resume point, worked out when the library was entered
    /// or changed (nothing is read from the disk while drawing).
    pub resumable: &'a HashSet<GameId>,
    /// There is a library to go back to.
    pub can_return: bool,
    pub toasts: &'a Toasts,
}

/// The result of one frame of egui.
pub struct Drawn {
    pub actions: Vec<UiAction>,
    /// When egui wants to be drawn again on its own (an animation, a tooltip); zero
    /// means now. `None`: only when something happens.
    pub repaint_after: Option<Duration>,
}

/// Which pop-up is open in front of the screen, and the confirmation asked for.
#[derive(Default)]
struct View {
    help: bool,
    /// The list of snapshots to load or delete.
    snapshots: bool,
    confirm: Option<Confirm>,
}

impl View {
    /// The help, the snapshot list or a confirmation is open.
    fn overlay_open(&self) -> bool {
        self.help || self.snapshots || self.confirm.is_some()
    }

    /// Close the front-most pop-up: a confirmation, else the snapshot list, else the help.
    fn close_front(&mut self) {
        if self.confirm.is_some() {
            self.confirm = None;
        } else if self.snapshots {
            self.snapshots = false;
        } else {
            self.help = false;
        }
    }
}

/// A question the player answers before something is removed or restarted.
enum Confirm {
    /// Take a game out of the library.
    Remove {
        id: GameId,
        title: String,
        delete_saves: bool,
    },
    /// Delete the snapshot taken at `secs`, which is shown as `when`.
    DeleteSnapshot { secs: u64, when: String },
    /// Power-cycle the running game.
    Restart,
}

impl Confirm {
    fn heading(&self) -> &'static str {
        match self {
            Confirm::Remove { .. } => "Remove from library?",
            Confirm::DeleteSnapshot { .. } => "Delete snapshot?",
            Confirm::Restart => "Restart game?",
        }
    }

    fn yes(&self) -> &'static str {
        match self {
            Confirm::Remove { .. } => "Remove",
            Confirm::DeleteSnapshot { .. } => "Delete",
            Confirm::Restart => "Restart",
        }
    }

    fn action(&self) -> UiAction {
        match self {
            Confirm::Remove {
                id, delete_saves, ..
            } => UiAction::Remove {
                id: id.clone(),
                delete_saves: *delete_saves,
            },
            Confirm::DeleteSnapshot { secs, .. } => UiAction::DeleteSnapshot(*secs),
            Confirm::Restart => UiAction::Restart,
        }
    }
}

pub struct Gui {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    view: View,
    thumbs: Thumbs,
    snap_thumbs: SnapThumbs,
}

/// How long a thumbnail that failed to load is left alone before the next try (seconds).
const THUMBNAIL_RETRY_SECS: f64 = 10.0;

/// Thumbnail textures by game. A game with none listed is not cached at all; one that
/// is listed but could not be read is tried again after a while and whenever the
/// screens are reset (the library is entered again).
type Thumbs = HashMap<GameId, Thumb>;

/// Textures of the snapshot pictures of the running game by snapshot time; `None` for a
/// snapshot with no picture (or one that cannot be read). Dropped when the list closes.
type SnapThumbs = HashMap<u64, Option<TextureHandle>>;

enum Thumb {
    Loaded(TextureHandle),
    /// Failed at this egui time (seconds).
    Failed(f64),
}

impl Gui {
    pub fn new(window: &Window, pixels: &Pixels<'_>) -> Self {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::Dark);
        ctx.global_style_mut(|style| {
            style.spacing.item_spacing = vec2(8.0, 8.0);
            style.spacing.button_padding = vec2(12.0, 6.0);
        });
        let mut state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        state.set_max_texture_side(pixels.device().limits().max_texture_dimension_2d as usize);
        // A plain (not sRGB) surface where there is one, so egui blends as it means to; see
        // `surface`.
        let renderer = egui_wgpu::Renderer::new(
            pixels.device(),
            pixels.surface_texture_format(),
            egui_wgpu::RendererOptions::default(),
        );
        Self {
            ctx,
            state,
            renderer,
            view: View::default(),
            thumbs: HashMap::new(),
            snap_thumbs: HashMap::new(),
        }
    }

    /// Give egui a window event (pointer, focus, resize; keys only while a screen is
    /// up). Returns whether egui wants a redraw.
    pub fn event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        self.state.on_window_event(window, event).repaint
    }

    /// Drop what egui has gathered, for the frames nothing is drawn (while playing).
    pub fn discard_input(&mut self, window: &Window) {
        drop(self.state.take_egui_input(window));
    }

    /// The help screen, the snapshot list or a confirmation is open in front of the menu:
    /// Esc closes it.
    pub fn overlay_open(&self) -> bool {
        self.view.overlay_open()
    }

    /// Close the front-most pop-up (see [`View::close_front`]).
    pub fn close_overlay(&mut self) {
        self.view.close_front();
        if !self.view.snapshots {
            self.snap_thumbs.clear();
        }
    }

    /// Close every pop-up (the screen changed).
    pub fn reset_view(&mut self) {
        self.view = View::default();
        self.snap_thumbs.clear();
        self.thumbs.retain(|_, t| matches!(t, Thumb::Loaded(_)));
    }

    /// Forget the picture of the snapshot taken at `secs` (it was deleted).
    pub fn forget_snapshot(&mut self, secs: u64) {
        self.snap_thumbs.remove(&secs);
    }

    /// Forget the cached thumbnail of `id` (it was set or the game removed).
    pub fn forget_thumbnail(&mut self, id: &GameId) {
        self.thumbs.remove(id);
    }

    /// Run the screens for one frame and draw them onto the window: over the game's
    /// picture when `mode` shows the game behind (paused, or a toast while playing), on
    /// a cleared surface in the library.
    pub fn draw(
        &mut self,
        window: &Window,
        pixels: &Pixels<'_>,
        screen: &Screen<'_>,
    ) -> Result<Drawn, pixels::Error> {
        let raw = self.state.take_egui_input(window);
        let mut actions = Vec::new();
        let Self {
            ctx,
            state,
            renderer,
            view,
            thumbs,
            snap_thumbs,
        } = self;
        let output = ctx.run_ui(raw, |ui| {
            build(ui, view, (thumbs, snap_thumbs), screen, &mut actions);
        });
        state.handle_platform_output(window, output.platform_output);
        let repaint_after = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay)
            .filter(|d| *d < Duration::from_secs(3600));

        // Textures are uploaded even if the surface turns out to be unavailable this
        // frame, so egui's idea of what the GPU holds stays right.
        for (id, delta) in &output.textures_delta.set {
            renderer.update_texture(pixels.device(), pixels.queue(), *id, delta);
        }
        let jobs = ctx.tessellate(output.shapes, output.pixels_per_point);
        let size = window.inner_size();
        let descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [size.width, size.height],
            pixels_per_point: output.pixels_per_point,
        };
        let library = screen.mode == Mode::Library;
        let rendered = pixels.render_with(|encoder, target, context| {
            drop(renderer.update_buffers(
                &context.device,
                &context.queue,
                encoder,
                &jobs,
                &descriptor,
            ));
            if !library {
                context.scaling_renderer.render(encoder, target);
            }
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("ui_render_pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: if library {
                                wgpu::LoadOp::Clear(wgpu::Color::BLACK)
                            } else {
                                wgpu::LoadOp::Load
                            },
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            renderer.render(&mut pass, &jobs, &descriptor);
            Ok(())
        });
        for id in &output.textures_delta.free {
            renderer.free_texture(id);
        }
        rendered?;
        Ok(Drawn {
            actions,
            repaint_after,
        })
    }
}

/// Decode a thumbnail PNG to RGBA8 and its size. `None` if it is not a PNG, is empty or
/// larger than [`MAX_THUMBNAIL_SIDE`]. Screenshots of a Game Boy are greyscale, of a
/// Game Boy Color or Advance RGB; both come out as RGBA.
pub fn decode_thumbnail(bytes: &[u8]) -> Option<([usize; 2], Vec<u8>)> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let (width, height) = (reader.info().width, reader.info().height);
    if width == 0 || height == 0 || width > MAX_THUMBNAIL_SIDE || height > MAX_THUMBNAIL_SIDE {
        return None;
    }
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let data = &buf[..info.buffer_size()];
    let rgba: Vec<u8> = match reader.output_color_type().0 {
        png::ColorType::Grayscale => data.iter().flat_map(|&g| [g, g, g, 0xFF]).collect(),
        png::ColorType::GrayscaleAlpha => data
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|&[g, a]| [g, g, g, a])
            .collect(),
        png::ColorType::Rgb => data
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 0xFF])
            .collect(),
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Indexed => return None,
    };
    let size = [width as usize, height as usize];
    (rgba.len() == size[0] * size[1] * 4).then_some((size, rgba))
}

// ---------------------------------------------------------------------------
// Screens
// ---------------------------------------------------------------------------

/// Everything on screen this frame.
fn build(
    ui: &mut egui::Ui,
    view: &mut View,
    (thumbs, snap_thumbs): (&mut Thumbs, &mut SnapThumbs),
    screen: &Screen<'_>,
    actions: &mut Vec<UiAction>,
) {
    let ctx = ui.ctx().clone();
    if screen.mode == Mode::Library {
        library_screen(ui, view, thumbs, screen, actions);
    }
    // One pop-up at a time: help, else a confirmation, else the snapshot list, else the
    // pause menu.
    if view.help {
        help_modal(&ctx, view, screen, actions);
    } else if view.confirm.is_some() {
        confirm_modal(&ctx, view, actions);
    } else if view.snapshots && screen.mode == Mode::Paused {
        snapshots_modal(&ctx, view, snap_thumbs, screen, actions);
    } else if screen.mode == Mode::Paused {
        pause_modal(&ctx, view, screen, actions);
    }
    toasts(&ctx, screen.toasts);
}

fn library_screen(
    ui: &mut egui::Ui,
    view: &mut View,
    thumbs: &mut Thumbs,
    screen: &Screen<'_>,
    actions: &mut Vec<UiAction>,
) {
    egui::Panel::top("library_top").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.heading("gameboyem");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let folder = egui::Button::new("Open data folder");
                if ui
                    .add_enabled(screen.data_dir.is_some(), folder)
                    .on_disabled_hover_text("There is no data folder this time")
                    .clicked()
                {
                    actions.push(UiAction::OpenDataFolder);
                }
                if ui.button("Help").clicked() {
                    view.help = true;
                }
                if ui.add(egui::Button::new("Add game").fill(ACCENT)).clicked() {
                    actions.push(UiAction::AddGames);
                }
            });
        });
    });
    egui::CentralPanel::default_margins().show(ui, |ui| {
        let entries = screen.library.map_or(&[][..], Library::entries);
        if screen.library.is_none() {
            ui.colored_label(
                MISSING,
                "There is no data folder, so there is no game library. Add game still plays a \
                 ROM, and its saves go next to it.",
            );
        }
        if entries.is_empty() {
            empty_library(ui, actions);
            return;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(CARD_GAP, CARD_GAP);
                ui.horizontal_wrapped(|ui| {
                    for index in library_order(entries) {
                        let entry = &entries[index];
                        let texture = match screen.library {
                            Some(library) => thumbnail(ui.ctx(), thumbs, library, entry),
                            None => None,
                        };
                        let resume = screen.resumable.contains(&entry.id);
                        card(ui, entry, texture, resume, &mut view.confirm, actions);
                    }
                });
                ui.add_space(8.0);
                ui.weak(
                    "Click a game to play it. Right-click a game to start it fresh or remove it.",
                );
            });
    });
}

fn empty_library(ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.25);
        ui.heading("No games yet");
        ui.label("Add Game Boy, Game Boy Color or Game Boy Advance ROMs (.gb, .gbc, .gba),");
        ui.label("or drag a ROM file onto this window.");
        ui.add_space(8.0);
        if ui
            .add_sized([160.0, 36.0], egui::Button::new("Add game").fill(ACCENT))
            .clicked()
        {
            actions.push(UiAction::AddGames);
        }
    });
}

/// The texture for `entry`'s thumbnail, loading it the first time it is needed (and again
/// some time after a failure).
fn thumbnail<'a>(
    ctx: &egui::Context,
    thumbs: &'a mut Thumbs,
    library: &Library,
    entry: &Entry,
) -> Option<&'a TextureHandle> {
    if entry.thumbnail.is_none() {
        thumbs.remove(&entry.id);
        return None;
    }
    let now = ctx.input(|i| i.time);
    if matches!(thumbs.get(&entry.id), Some(Thumb::Failed(at)) if now - *at >= THUMBNAIL_RETRY_SECS)
    {
        thumbs.remove(&entry.id);
    }
    let loaded = thumbs.entry(entry.id.clone()).or_insert_with(|| {
        let texture = library
            .read_thumbnail(&entry.id)
            .and_then(|bytes| decode_thumbnail(&bytes))
            .map(|(size, rgba)| {
                ctx.load_texture(
                    format!("thumbnail-{}", entry.id),
                    egui::ColorImage::from_rgba_unmultiplied(size, &rgba),
                    TextureOptions::NEAREST,
                )
            });
        texture.map_or(Thumb::Failed(now), Thumb::Loaded)
    });
    match loaded {
        Thumb::Loaded(texture) => Some(texture),
        Thumb::Failed(_) => None,
    }
}

fn card(
    ui: &mut egui::Ui,
    entry: &Entry,
    texture: Option<&TextureHandle>,
    resume: bool,
    confirm: &mut Option<Confirm>,
    actions: &mut Vec<UiAction>,
) {
    let (rect, response) = ui.allocate_exact_size(vec2(CARD_WIDTH, CARD_HEIGHT), Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    let fill = if response.hovered() {
        visuals.widgets.hovered.bg_fill
    } else {
        visuals.widgets.inactive.bg_fill
    };
    let stroke_color = if response.hovered() {
        ACCENT
    } else {
        visuals.widgets.noninteractive.bg_stroke.color
    };
    let text_color = visuals.text_color();
    let round = egui::CornerRadius::same(8);
    painter.rect_filled(rect, round, fill);
    painter.rect_stroke(rect, round, (1.0, stroke_color), egui::StrokeKind::Inside);

    // The picture, kept in proportion inside its box; or the console's name.
    let thumb = Rect::from_min_size(
        rect.min + vec2(CARD_PAD, CARD_PAD),
        vec2(CARD_WIDTH - 2.0 * CARD_PAD, THUMB_HEIGHT),
    );
    painter.rect_filled(thumb, egui::CornerRadius::same(4), Color32::from_gray(18));
    match texture {
        Some(texture) => {
            let size = texture.size_vec2();
            let scale = (thumb.width() / size.x).min(thumb.height() / size.y);
            let shown = Rect::from_center_size(thumb.center(), size * scale);
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            let tint = if entry.available {
                Color32::WHITE
            } else {
                Color32::from_gray(90)
            };
            painter.image(texture.id(), shown, uv, tint);
        }
        None => {
            painter.text(
                thumb.center(),
                Align2::CENTER_CENTER,
                entry.system.label(),
                FontId::proportional(34.0),
                Color32::from_gray(110),
            );
        }
    }

    // The title, cut to one line.
    let mut job = egui::text::LayoutJob::single_section(
        entry.title.clone(),
        egui::text::TextFormat {
            font_id: FontId::proportional(14.0),
            color: text_color,
            ..Default::default()
        },
    );
    job.wrap = egui::text::TextWrapping::truncate_at_width(CARD_WIDTH - 2.0 * CARD_PAD);
    let galley = painter.layout_job(job);
    painter.galley(
        pos2(rect.min.x + CARD_PAD, thumb.max.y + 8.0),
        galley,
        text_color,
    );

    // Badges: the console, then "Bundled", "Resume" or "missing".
    let mut x = rect.min.x + CARD_PAD;
    let y = rect.max.y - CARD_PAD - 18.0;
    let mut badge = |label: &str, color: Color32| {
        let galley =
            painter.layout_no_wrap(label.to_owned(), FontId::proportional(11.0), Color32::WHITE);
        let badge = Rect::from_min_size(pos2(x, y), galley.size() + vec2(10.0, 4.0));
        painter.rect_filled(badge, egui::CornerRadius::same(4), color);
        painter.galley(badge.min + vec2(5.0, 2.0), galley, Color32::WHITE);
        x = badge.max.x + 6.0;
    };
    badge(entry.system.label(), BADGE);
    if entry.source == Source::Bundled {
        badge("Bundled", ACCENT);
    }
    if resume {
        badge("Resume", ACCENT);
    }
    if !entry.available {
        badge("missing", MISSING);
    }

    let response = match entry.last_played {
        Some(secs) => response.on_hover_ui(|ui| {
            let resumes = if resume {
                " It resumes where you left off."
            } else {
                ""
            };
            ui.label(format!(
                "Last played {}.{resumes}",
                friendly_time_local(secs)
            ));
        }),
        None => response,
    };
    response.context_menu(|ui| {
        let fresh = egui::Button::new("Start fresh");
        if ui
            .add_enabled(entry.available, fresh)
            .on_hover_text(
                "Forget where you left off and start from the beginning. The game's own save is kept.",
            )
            .on_disabled_hover_text("The ROM is missing")
            .clicked()
        {
            actions.push(UiAction::StartFresh(entry.id.clone()));
            ui.close();
        }
        if entry.source == Source::Bundled {
            ui.add_enabled(false, egui::Button::new("Remove from library"))
                .on_disabled_hover_text("Bundled games come with the app and cannot be removed");
        } else if ui.button("Remove from library").clicked() {
            *confirm = Some(Confirm::Remove {
                id: entry.id.clone(),
                title: entry.title.clone(),
                delete_saves: false,
            });
            ui.close();
        }
    });
    if response.clicked() {
        actions.push(UiAction::Play(entry.id.clone()));
    }
}

fn menu_button(ui: &mut egui::Ui, text: &str) -> bool {
    ui.add_sized([ui.available_width(), 32.0], egui::Button::new(text))
        .clicked()
}

fn pause_modal(
    ctx: &egui::Context,
    view: &mut View,
    screen: &Screen<'_>,
    actions: &mut Vec<UiAction>,
) {
    egui::Modal::new(Id::new("pause_menu")).show(ctx, |ui| {
        ui.set_width(280.0);
        ui.vertical_centered(|ui| {
            ui.heading("Paused");
            if !screen.game_title.is_empty() {
                ui.weak(screen.game_title);
            }
        });
        ui.add_space(4.0);
        if menu_button(ui, "Resume") {
            actions.push(UiAction::Resume);
        }
        if screen.can_save || screen.can_restart {
            ui.separator();
        }
        if screen.can_save {
            if menu_button(ui, "Save snapshot (K)") {
                actions.push(UiAction::SaveSnapshot);
            }
            let load = egui::Button::new("Load snapshot\u{2026}");
            let none = screen.snapshots.is_empty();
            if ui
                .add_enabled_ui(!none, |ui| {
                    ui.add_sized([ui.available_width(), 32.0], load)
                        .on_disabled_hover_text("No snapshots yet")
                })
                .inner
                .clicked()
            {
                view.snapshots = true;
            }
        }
        if screen.can_restart && menu_button(ui, "Restart game") {
            view.confirm = Some(Confirm::Restart);
        }
        if screen.can_save && menu_button(ui, "Screenshot (C)") {
            actions.push(UiAction::Screenshot);
        }
        if screen.can_save || screen.can_restart {
            ui.separator();
        }
        if menu_button(ui, "Help") {
            view.help = true;
        }
        if screen.can_return && menu_button(ui, "Back to library") {
            actions.push(UiAction::BackToLibrary);
        }
        if menu_button(ui, "Quit") {
            actions.push(UiAction::Quit);
        }
    });
}

/// The picture of a snapshot, loaded the first time its row is drawn.
fn snapshot_texture<'a>(
    ctx: &egui::Context,
    cache: &'a mut SnapThumbs,
    item: &SnapshotItem,
) -> Option<&'a TextureHandle> {
    cache
        .entry(item.secs)
        .or_insert_with(|| {
            let bytes = read_capped(item.image.as_ref()?, MAX_IMAGE_BYTES, "snapshot picture")
                .ok()
                .flatten()?;
            let (size, rgba) = decode_thumbnail(&bytes)?;
            Some(ctx.load_texture(
                format!("snapshot-{}", item.secs),
                egui::ColorImage::from_rgba_unmultiplied(size, &rgba),
                TextureOptions::NEAREST,
            ))
        })
        .as_ref()
}

fn snapshots_modal(
    ctx: &egui::Context,
    view: &mut View,
    cache: &mut SnapThumbs,
    screen: &Screen<'_>,
    actions: &mut Vec<UiAction>,
) {
    let height = (ctx.content_rect().height() - 200.0).max(SNAP_HEIGHT);
    let width = (ctx.content_rect().width() - 80.0).clamp(280.0, 460.0);
    let mut close = false;
    egui::Modal::new(Id::new("snapshots")).show(ctx, |ui| {
        ui.set_width(width);
        ui.heading("Load snapshot");
        if screen.snapshots.is_empty() {
            ui.label("No snapshots yet. Press K while playing, or choose Save snapshot.");
        } else {
            egui::ScrollArea::vertical()
                .max_height(height)
                .auto_shrink([false, true])
                .show_rows(ui, SNAP_HEIGHT, screen.snapshots.len(), |ui, rows| {
                    for item in &screen.snapshots[rows] {
                        let texture = snapshot_texture(ui.ctx(), cache, item);
                        snapshot_row(ui, item, texture, &mut view.confirm, actions);
                    }
                });
        }
        ui.add_space(4.0);
        if ui.button("Back").clicked() {
            close = true;
        }
    });
    if close {
        view.snapshots = false;
        cache.clear();
    }
}

/// One snapshot: its picture (or a placeholder), when it was taken, Load and Delete.
fn snapshot_row(
    ui: &mut egui::Ui,
    item: &SnapshotItem,
    texture: Option<&TextureHandle>,
    confirm: &mut Option<Confirm>,
    actions: &mut Vec<UiAction>,
) {
    let size = vec2(ui.available_width(), SNAP_HEIGHT);
    ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Center), |ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(SNAP_WIDTH, SNAP_HEIGHT), Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, egui::CornerRadius::same(4), Color32::from_gray(18));
        match texture {
            Some(texture) => {
                let size = texture.size_vec2();
                let scale = (rect.width() / size.x).min(rect.height() / size.y);
                let shown = Rect::from_center_size(rect.center(), size * scale);
                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                painter.image(texture.id(), shown, uv, Color32::WHITE);
            }
            None => {
                painter.text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    "No picture",
                    FontId::proportional(11.0),
                    Color32::from_gray(110),
                );
            }
        }
        ui.label(RichText::new(&item.when).strong());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Load").clicked() {
                actions.push(UiAction::LoadSnapshot(item.secs));
            }
            if ui.button("Delete").clicked() {
                *confirm = Some(Confirm::DeleteSnapshot {
                    secs: item.secs,
                    when: item.when.clone(),
                });
            }
        });
    });
}

fn confirm_modal(ctx: &egui::Context, view: &mut View, actions: &mut Vec<UiAction>) {
    let Some(confirm) = view.confirm.as_mut() else {
        return;
    };
    let mut done = false;
    egui::Modal::new(Id::new("confirm")).show(ctx, |ui| {
        ui.set_width(340.0);
        ui.heading(confirm.heading());
        match confirm {
            Confirm::Remove {
                title,
                delete_saves,
                ..
            } => {
                ui.label(format!(
                    "{title} will be taken out of the library and its ROM copy deleted. Your \
                     original file is not touched."
                ));
                ui.checkbox(
                    delete_saves,
                    "Also delete its saves, resume point, snapshots and screenshots",
                );
            }
            Confirm::DeleteSnapshot { when, .. } => {
                ui.label(format!(
                    "The snapshot from {when} will be deleted. This cannot be undone."
                ));
            }
            Confirm::Restart => {
                ui.label(
                    "The game starts over from the beginning, as if it were switched off and \
                     on again. Its own save and your snapshots are kept.",
                );
            }
        }
        ui.horizontal(|ui| {
            if ui.button(confirm.yes()).clicked() {
                actions.push(confirm.action());
                done = true;
            }
            if ui.button("Cancel").clicked() {
                done = true;
            }
        });
    });
    if done {
        view.confirm = None;
    }
}

fn help_modal(
    ctx: &egui::Context,
    view: &mut View,
    screen: &Screen<'_>,
    actions: &mut Vec<UiAction>,
) {
    let height = (ctx.content_rect().height() - 140.0).max(160.0);
    let width = (ctx.content_rect().width() - 80.0).clamp(280.0, 540.0);
    egui::Modal::new(Id::new("help")).show(ctx, |ui| {
        ui.set_width(width);
        ui.heading("Help");
        egui::ScrollArea::vertical()
            .max_height(height)
            .auto_shrink([false, true])
            .show(ui, |ui| help_text(ui, screen, actions));
        ui.add_space(4.0);
        if ui.button("Close").clicked() {
            view.help = false;
        }
    });
}

fn help_text(ui: &mut egui::Ui, screen: &Screen<'_>, actions: &mut Vec<UiAction>) {
    ui.strong("Adding games");
    ui.label(
        "Click Add game and choose .gb, .gbc or .gba files to put them in your library. \
         Or drag a ROM onto the window at any time: it starts playing and is added to \
         the library.",
    );
    ui.add_space(6.0);

    ui.strong("Saving");
    for (name, what) in [
        (
            "The game's own save.",
            "Happens when the game saves, for example at a save point. It is kept \
             automatically.",
        ),
        (
            "Resume.",
            "When you leave a game, the app remembers exactly where you were and picks up \
             there next time. Right-click a game and choose Start fresh to begin again.",
        ),
        (
            "Snapshots.",
            "Save anywhere with K or the pause menu, and load with L (the latest one) or \
             the menu (any of them).",
        ),
    ] {
        ui.horizontal_wrapped(|ui| {
            ui.strong(name);
            ui.label(what);
        });
    }
    ui.add_space(6.0);

    ui.strong("Controls");
    egui::Grid::new("help_controls")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            for row in keymap::help_rows() {
                ui.label(row.what);
                ui.label(RichText::new(row.keys.join(" / ")).monospace());
                ui.end_row();
            }
        });
    ui.weak("In menus, click, or use Tab and Enter. Esc goes back.");
    ui.add_space(6.0);

    ui.strong("Where your files are");
    match screen.data_dir {
        Some(dir) => {
            ui.label(RichText::new(dir.display().to_string()).monospace());
            ui.label(
                "saves/ holds in-game saves, states/ your resume points and snapshots, \
                 screenshots/ your screenshots, games/ the library, and logs/gameboyem.log \
                 the log.",
            );
            if ui.button("Open folder").clicked() {
                actions.push(UiAction::OpenDataFolder);
            }
        }
        None => {
            ui.label(
                "There is no data folder this time, so saves, resume points, snapshots and \
                 screenshots go next to the ROM.",
            );
        }
    }
    ui.add_space(6.0);

    ui.strong("Bundled games");
    ui.label(
        "Games marked Bundled come with the app. They cannot be removed, and your saves \
         for them are kept like any other game's.",
    );
    ui.add_space(6.0);

    ui.strong("About the warning when you first open the app");
    ui.label(
        "The app is not code-signed yet. On macOS, right-click it, choose Open, then Open \
         again. On Windows, at \"Windows protected your PC\", click More info, then Run anyway.",
    );
}

fn toasts(ctx: &egui::Context, toasts: &Toasts) {
    if toasts.is_empty() {
        return;
    }
    egui::Area::new(Id::new("toasts"))
        .anchor(Align2::RIGHT_BOTTOM, vec2(-12.0, -12.0))
        .order(egui::Order::Tooltip)
        .interactable(false)
        .show(ctx, |ui| {
            ui.set_max_width(340.0);
            for toast in toasts.iter() {
                let color = match toast.level {
                    log::Level::Error => MISSING,
                    log::Level::Warn => Color32::from_rgb(0xe0, 0xb0, 0x40),
                    _ => ui.visuals().text_color(),
                };
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.colored_label(color, &toast.text);
                });
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::DataDir;
    use crate::record::encode_png;
    use emu_common::{FrameRef, Pixels as FramePixels};
    use std::fs;

    #[test]
    fn a_shade_png_decodes_to_grey_rgba() {
        let shades: Vec<u8> = (0..160 * 144).map(|i| (i % 4) as u8).collect();
        let png = encode_png(FrameRef {
            width: 160,
            height: 144,
            pixels: FramePixels::Shades(&shades),
        })
        .unwrap();
        let (size, rgba) = decode_thumbnail(&png).unwrap();
        assert_eq!(size, [160, 144]);
        assert_eq!(rgba.len(), 160 * 144 * 4);
        assert_eq!(&rgba[0..4], &[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(&rgba[12..16], &[0x00, 0x00, 0x00, 0xFF]);
    }

    #[test]
    fn a_colour_png_decodes_to_rgba() {
        let px: Vec<u16> = (0..240 * 160).map(|_| 0x001F).collect(); // red
        let png = encode_png(FrameRef {
            width: 240,
            height: 160,
            pixels: FramePixels::Rgb555(&px),
        })
        .unwrap();
        let (size, rgba) = decode_thumbnail(&png).unwrap();
        assert_eq!(size, [240, 160]);
        assert_eq!(&rgba[0..4], &[0xFF, 0, 0, 0xFF]);
    }

    #[test]
    fn something_that_is_not_a_png_is_no_thumbnail() {
        assert!(decode_thumbnail(b"not a png").is_none());
        assert!(decode_thumbnail(&[]).is_none());
    }

    fn rom(title: &str) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x134..0x134 + title.len()].copy_from_slice(title.as_bytes());
        rom
    }

    /// Run `screen` through egui for a few frames with no window or GPU, and return what
    /// the player asked for. Catches panics and layout asserts, not looks.
    fn run(screen: &Screen<'_>, view: &mut View) -> Vec<UiAction> {
        let ctx = egui::Context::default();
        let mut thumbs = HashMap::new();
        let mut snap_thumbs = HashMap::new();
        let mut actions = Vec::new();
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 650.0))),
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| {
                build(
                    ui,
                    view,
                    (&mut thumbs, &mut snap_thumbs),
                    screen,
                    &mut actions,
                );
            });
        }
        actions
    }

    fn items() -> Vec<SnapshotItem> {
        (0..60)
            .map(|n| SnapshotItem {
                secs: 1_700_000_000 + n,
                when: format!("Today {n}"),
                image: None,
            })
            .collect()
    }

    #[test]
    fn every_screen_draws_without_asking_for_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDir::at(tmp.path().join("data")).created().unwrap();
        let mut library = Library::open(&data);
        for title in ["ONE", "TWO"] {
            let path = tmp.path().join(format!("{title}.gb"));
            fs::write(&path, rom(title)).unwrap();
            let id = library.add_rom(&path).unwrap().entry.id;
            // One game that resumes, one that does not.
            if title == "ONE" {
                let paths = library.paths_for(&id);
                fs::create_dir_all(paths.states_dir()).unwrap();
                fs::write(paths.resume_state(), b"x").unwrap();
            }
        }
        let toasts = Toasts::default();
        let snapshots = items();
        let resumable: HashSet<GameId> = library
            .entries()
            .iter()
            .filter(|e| library.has_resume(&e.id))
            .map(|e| e.id.clone())
            .collect();
        assert_eq!(resumable.len(), 1, "ONE resumes, TWO does not");
        let screen = |mode, library| Screen {
            mode,
            library,
            data_dir: Some(data.root()),
            game_title: "ONE",
            can_save: true,
            can_restart: true,
            snapshots: &snapshots,
            resumable: &resumable,
            can_return: true,
            toasts: &toasts,
        };

        let confirms = || {
            [
                Confirm::Remove {
                    id: GameId::new("ONE", 1),
                    title: "ONE".to_owned(),
                    delete_saves: false,
                },
                Confirm::DeleteSnapshot {
                    secs: 5,
                    when: "Today 5:41 PM".to_owned(),
                },
                Confirm::Restart,
            ]
        };
        for view in [
            View::default(),
            View {
                help: true,
                ..View::default()
            },
        ]
        .into_iter()
        .chain(confirms().map(|c| View {
            confirm: Some(c),
            ..View::default()
        })) {
            let mut view = view;
            assert!(run(&screen(Mode::Library, Some(&library)), &mut view).is_empty());
        }
        let mut view = View::default();
        assert!(run(&screen(Mode::Paused, Some(&library)), &mut view).is_empty());
        // The snapshot list, with rows, with none, and with a question in front of it.
        let mut view = View {
            snapshots: true,
            ..View::default()
        };
        assert!(run(&screen(Mode::Paused, Some(&library)), &mut view).is_empty());
        let none = Screen {
            snapshots: &[],
            resumable: &HashSet::new(),
            ..screen(Mode::Paused, None)
        };
        assert!(run(&none, &mut view).is_empty());
        for c in confirms() {
            let mut view = View {
                snapshots: true,
                confirm: Some(c),
                ..View::default()
            };
            assert!(run(&screen(Mode::Paused, None), &mut view).is_empty());
        }
        let mut view = View {
            help: true,
            ..View::default()
        };
        assert!(run(&screen(Mode::Paused, None), &mut view).is_empty());
        // The empty library and the one without a data folder.
        let mut view = View::default();
        assert!(run(&screen(Mode::Library, None), &mut view).is_empty());
    }

    #[test]
    fn the_pause_menu_has_no_slot_picker_and_names_the_snapshot_keys() {
        // Everything the menu says, found by drawing it and reading the text out.
        let toasts = Toasts::default();
        let snapshots = items();
        let screen = Screen {
            mode: Mode::Paused,
            library: None,
            data_dir: None,
            game_title: "ONE",
            can_save: true,
            can_restart: true,
            snapshots: &snapshots,
            resumable: &HashSet::new(),
            can_return: true,
            toasts: &toasts,
        };
        let ctx = egui::Context::default();
        let (mut view, mut thumbs, mut snap_thumbs, mut actions) =
            (View::default(), HashMap::new(), HashMap::new(), Vec::new());
        let mut text = String::new();
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 650.0))),
                ..Default::default()
            };
            let out = ctx.run_ui(input, |ui| {
                build(
                    ui,
                    &mut view,
                    (&mut thumbs, &mut snap_thumbs),
                    &screen,
                    &mut actions,
                );
            });
            text = out
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::epaint::Shape::Text(t) => Some(t.galley.text().to_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("|");
        }
        for wanted in [
            "Resume",
            "Save snapshot (K)",
            "Load snapshot\u{2026}",
            "Restart game",
            "Screenshot (C)",
            "Help",
            "Back to library",
            "Quit",
        ] {
            assert!(text.contains(wanted), "{wanted} is missing from {text}");
        }
        assert!(!text.to_lowercase().contains("slot"), "{text}");
    }

    /// Only the games in `resumable` get the Resume tag, and drawing reads nothing from disk.
    #[test]
    fn the_resume_tag_follows_the_set_the_app_worked_out() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDir::at(tmp.path().join("data")).created().unwrap();
        let mut library = Library::open(&data);
        let mut ids = Vec::new();
        for title in ["ONE", "TWO"] {
            let path = tmp.path().join(format!("{title}.gb"));
            fs::write(&path, rom(title)).unwrap();
            ids.push(library.add_rom(&path).unwrap().entry.id);
        }
        // A resume point on disk that the app has not told the screen about.
        let paths = library.paths_for(&ids[1]);
        fs::create_dir_all(paths.states_dir()).unwrap();
        fs::write(paths.resume_state(), b"x").unwrap();

        let toasts = Toasts::default();
        let tags = |resumable: &HashSet<GameId>| {
            let screen = Screen {
                mode: Mode::Library,
                library: Some(&library),
                data_dir: None,
                game_title: "",
                can_save: false,
                can_restart: false,
                snapshots: &[],
                resumable,
                can_return: false,
                toasts: &toasts,
            };
            let ctx = egui::Context::default();
            let (mut view, mut thumbs, mut snap_thumbs, mut actions) =
                (View::default(), HashMap::new(), HashMap::new(), Vec::new());
            let mut count = 0;
            for _ in 0..2 {
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 650.0))),
                    ..Default::default()
                };
                let out = ctx.run_ui(input, |ui| {
                    build(
                        ui,
                        &mut view,
                        (&mut thumbs, &mut snap_thumbs),
                        &screen,
                        &mut actions,
                    );
                });
                count = out
                    .shapes
                    .iter()
                    .filter(|s| {
                        matches!(&s.shape, egui::epaint::Shape::Text(t) if t.galley.text() == "Resume")
                    })
                    .count();
            }
            count
        };
        assert_eq!(tags(&HashSet::new()), 0, "the set decides, not the disk");
        assert_eq!(tags(&HashSet::from([ids[0].clone()])), 1);
        assert_eq!(tags(&ids.iter().cloned().collect()), 2);
    }

    #[test]
    fn esc_closes_the_front_most_pop_up_first() {
        let mut view = View {
            snapshots: true,
            confirm: Some(Confirm::Restart),
            ..View::default()
        };
        assert!(view.overlay_open());
        view.close_front();
        assert!(view.snapshots && view.confirm.is_none());
        view.close_front();
        assert!(!view.snapshots && !view.overlay_open());
    }

    #[test]
    fn toasts_draw() {
        let mut toasts = Toasts::default();
        let now = std::time::Instant::now();
        toasts.push(log::Level::Error, "bad ROM", now);
        toasts.push(log::Level::Warn, "no data folder", now);
        toasts.push(log::Level::Info, "added", now);
        let screen = Screen {
            mode: Mode::Playing,
            library: None,
            data_dir: None,
            game_title: "",
            can_save: false,
            can_restart: false,
            snapshots: &[],
            resumable: &HashSet::new(),
            can_return: false,
            toasts: &toasts,
        };
        assert!(run(&screen, &mut View::default()).is_empty());
    }

    #[test]
    fn a_thumbnail_that_failed_to_load_is_tried_again_when_the_library_is_entered() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDir::at(tmp.path().join("data")).created().unwrap();
        let mut library = Library::open(&data);
        let path = tmp.path().join("one.gb");
        fs::write(&path, rom("ONE")).unwrap();
        let id = library.add_rom(&path).unwrap().entry.id;
        let shades = vec![1u8; 160 * 144];
        let png = encode_png(FrameRef {
            width: 160,
            height: 144,
            pixels: FramePixels::Shades(&shades),
        })
        .unwrap();
        let file = library.set_thumbnail(&id, &png).unwrap();
        let entry = library.get(&id).unwrap().clone();

        fs::write(&file, b"junk").unwrap();
        let ctx = egui::Context::default();
        let mut thumbs = Thumbs::new();
        assert!(thumbnail(&ctx, &mut thumbs, &library, &entry).is_none());
        fs::write(&file, &png).unwrap();
        assert!(
            thumbnail(&ctx, &mut thumbs, &library, &entry).is_none(),
            "left alone for a while"
        );
        // Entering the library again clears the failure.
        thumbs.retain(|_, t| matches!(t, Thumb::Loaded(_)));
        assert!(thumbnail(&ctx, &mut thumbs, &library, &entry).is_some());
    }
}
