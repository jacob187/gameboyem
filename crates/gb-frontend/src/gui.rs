//! The screens, drawn with egui into the window the game is shown in: the game list,
//! the pause menu and the help. egui draws over (or, in the library, instead of) the
//! picture, and only while a screen or a toast is up; see `App` for when.
//!
//! Drawing never does the work itself. A click is collected as a [`UiAction`] and the
//! app carries it out after the frame, so a file dialog or a file write never runs in
//! the middle of drawing.

use std::collections::HashMap;
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
use crate::ui::{Mode, Toasts, UiAction, library_order};

/// Largest thumbnail side drawn; the library refuses larger ones when they are set, and
/// a file placed in the folder by hand is held to the same limit.
const MAX_THUMBNAIL_SIDE: u32 = 1024;

const CARD_WIDTH: f32 = 176.0;
const CARD_HEIGHT: f32 = 188.0;
const CARD_PAD: f32 = 8.0;
const THUMB_HEIGHT: f32 = 128.0;
const CARD_GAP: f32 = 12.0;

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
    /// The selected save-state slot.
    pub slot: u8,
    pub toasts: &'a Toasts,
}

/// The result of one frame of egui.
pub struct Drawn {
    pub actions: Vec<UiAction>,
    /// When egui wants to be drawn again on its own (an animation, a tooltip); zero
    /// means now. `None`: only when something happens.
    pub repaint_after: Option<Duration>,
}

/// Which pop-up is open in front of the screen, and the library's confirmation.
#[derive(Default)]
struct View {
    help: bool,
    confirm: Option<Confirm>,
}

struct Confirm {
    id: GameId,
    title: String,
    delete_saves: bool,
}

pub struct Gui {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    view: View,
    /// Thumbnail textures by game. `None`: the game has none (or it cannot be read);
    /// not tried again until [`Gui::forget_thumbnail`].
    thumbs: HashMap<GameId, Option<TextureHandle>>,
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
        // The surface format pixels chose; egui-wgpu draws to either kind.
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

    /// The help screen or a confirmation is open in front of the menu: Esc closes it.
    pub fn overlay_open(&self) -> bool {
        self.view.help || self.view.confirm.is_some()
    }

    pub fn close_overlay(&mut self) {
        if self.view.confirm.is_some() {
            self.view.confirm = None;
        } else {
            self.view.help = false;
        }
    }

    /// Close every pop-up (the screen changed).
    pub fn reset_view(&mut self) {
        self.view = View::default();
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
        } = self;
        let output = ctx.run_ui(raw, |ui| build(ui, view, thumbs, screen, &mut actions));
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
    thumbs: &mut HashMap<GameId, Option<TextureHandle>>,
    screen: &Screen<'_>,
    actions: &mut Vec<UiAction>,
) {
    let ctx = ui.ctx().clone();
    if screen.mode == Mode::Library {
        library_screen(ui, view, thumbs, screen, actions);
    }
    // One pop-up at a time: help, else a confirmation, else the pause menu.
    if view.help {
        help_modal(&ctx, view, screen, actions);
    } else if view.confirm.is_some() {
        confirm_modal(&ctx, view, actions);
    } else if screen.mode == Mode::Paused {
        pause_modal(&ctx, view, screen, actions);
    }
    toasts(&ctx, screen.toasts);
}

fn library_screen(
    ui: &mut egui::Ui,
    view: &mut View,
    thumbs: &mut HashMap<GameId, Option<TextureHandle>>,
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
                        card(ui, entry, texture, &mut view.confirm, actions);
                    }
                });
                ui.add_space(8.0);
                ui.weak("Click a game to play it. Right-click a game to remove it.");
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

/// The texture for `entry`'s thumbnail, loading it the first time it is needed.
fn thumbnail<'a>(
    ctx: &egui::Context,
    thumbs: &'a mut HashMap<GameId, Option<TextureHandle>>,
    library: &Library,
    entry: &Entry,
) -> Option<&'a TextureHandle> {
    thumbs
        .entry(entry.id.clone())
        .or_insert_with(|| {
            let bytes = entry
                .thumbnail
                .as_ref()
                .and_then(|_| library.read_thumbnail(&entry.id))?;
            let (size, rgba) = decode_thumbnail(&bytes)?;
            Some(ctx.load_texture(
                format!("thumbnail-{}", entry.id),
                egui::ColorImage::from_rgba_unmultiplied(size, &rgba),
                TextureOptions::NEAREST,
            ))
        })
        .as_ref()
}

fn card(
    ui: &mut egui::Ui,
    entry: &Entry,
    texture: Option<&TextureHandle>,
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

    // Badges: the console, then "Bundled" or "missing".
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
    if !entry.available {
        badge("missing", MISSING);
    }

    response.context_menu(|ui| {
        if entry.source == Source::Bundled {
            ui.add_enabled(false, egui::Button::new("Remove from library"))
                .on_disabled_hover_text("Bundled games come with the app and cannot be removed");
        } else if ui.button("Remove from library").clicked() {
            *confirm = Some(Confirm {
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
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("State slot");
            for slot in crate::state::FIRST_SLOT..=crate::state::LAST_SLOT {
                if ui
                    .selectable_label(screen.slot == slot, slot.to_string())
                    .clicked()
                {
                    actions.push(UiAction::SelectSlot(slot));
                }
            }
        });
        if menu_button(ui, &format!("Save state (slot {})", screen.slot)) {
            actions.push(UiAction::SaveState);
        }
        if menu_button(ui, &format!("Load state (slot {})", screen.slot)) {
            actions.push(UiAction::LoadState);
        }
        if menu_button(ui, "Screenshot") {
            actions.push(UiAction::Screenshot);
        }
        ui.separator();
        if menu_button(ui, "Help") {
            view.help = true;
        }
        if menu_button(ui, "Back to library") {
            actions.push(UiAction::BackToLibrary);
        }
        if menu_button(ui, "Quit") {
            actions.push(UiAction::Quit);
        }
    });
}

fn confirm_modal(ctx: &egui::Context, view: &mut View, actions: &mut Vec<UiAction>) {
    let Some(confirm) = view.confirm.as_mut() else {
        return;
    };
    let mut done = false;
    egui::Modal::new(Id::new("confirm_remove")).show(ctx, |ui| {
        ui.set_width(340.0);
        ui.heading("Remove from library?");
        ui.label(format!(
            "{} will be taken out of the library and its ROM copy deleted. Your original \
             file is not touched.",
            confirm.title
        ));
        ui.checkbox(
            &mut confirm.delete_saves,
            "Also delete its saves, save states and screenshots",
        );
        ui.horizontal(|ui| {
            if ui.button("Remove").clicked() {
                actions.push(UiAction::Remove {
                    id: confirm.id.clone(),
                    delete_saves: confirm.delete_saves,
                });
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
        "Click Add game and choose .gb, .gbc or .gba files, or drag a ROM onto the window \
         at any time. It is copied into your library and starts playing.",
    );
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
                "saves/ holds in-game saves, states/ save states, screenshots/ your \
                 screenshots, games/ the library, and logs/gameboyem.log the log.",
            );
            if ui.button("Open folder").clicked() {
                actions.push(UiAction::OpenDataFolder);
            }
        }
        None => {
            ui.label(
                "There is no data folder this time, so saves, states and screenshots go \
                 next to the ROM.",
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
        let mut actions = Vec::new();
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 650.0))),
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| {
                build(ui, view, &mut thumbs, screen, &mut actions);
            });
        }
        actions
    }

    #[test]
    fn every_screen_draws_without_asking_for_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let data = DataDir::at(tmp.path().join("data")).created().unwrap();
        let mut library = Library::open(&data);
        for title in ["ONE", "TWO"] {
            let path = tmp.path().join(format!("{title}.gb"));
            fs::write(&path, rom(title)).unwrap();
            library.add_rom(&path).unwrap();
        }
        let toasts = Toasts::default();
        let screen = |mode, library| Screen {
            mode,
            library,
            data_dir: Some(data.root()),
            game_title: "ONE",
            slot: 2,
            toasts: &toasts,
        };

        for view in [
            View::default(),
            View {
                help: true,
                confirm: None,
            },
            View {
                help: false,
                confirm: Some(Confirm {
                    id: GameId::new("ONE", 1),
                    title: "ONE".to_owned(),
                    delete_saves: false,
                }),
            },
        ] {
            let mut view = view;
            assert!(run(&screen(Mode::Library, Some(&library)), &mut view).is_empty());
        }
        let mut view = View::default();
        assert!(run(&screen(Mode::Paused, Some(&library)), &mut view).is_empty());
        let mut view = View {
            help: true,
            confirm: None,
        };
        assert!(run(&screen(Mode::Paused, None), &mut view).is_empty());
        // The empty library and the one without a data folder.
        let mut view = View::default();
        assert!(run(&screen(Mode::Library, None), &mut view).is_empty());
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
            slot: 1,
            toasts: &toasts,
        };
        assert!(run(&screen, &mut View::default()).is_empty());
    }
}
