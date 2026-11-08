//! The formats of the window's surface and of the texture the game's picture is copied to.
//!
//! egui works in gamma space: its colours are sRGB-encoded values and it means them to be
//! blended as they are. On an sRGB surface (`*Srgb`) egui-wgpu turns each colour back to
//! linear in its shader before the GPU encodes it again, so a solid colour still comes out
//! right, but the GPU then blends in linear light: the dimmed backdrop behind a menu, the
//! edges of text, shadows and anything else translucent come out lighter than egui draws
//! them (washed out), and egui-wgpu warns that it prefers `Rgba8Unorm` or `Bgra8Unorm`.
//!
//! The game's picture must not change. pixels copies the frame's RGBA bytes into a texture
//! and draws it onto the surface with nearest sampling and, the picture being opaque, no
//! blending. With an sRGB texture on an sRGB surface the GPU decodes each byte to linear as
//! it samples and encodes it back as it writes, which gives the same byte. With a plain
//! texture on a plain surface neither step happens and the byte is copied as it is. The two
//! must change together: a plain texture on an sRGB surface would brighten the picture and
//! an sRGB texture on a plain surface would darken it.
//!
//! pixels cannot give egui a plain view of an sRGB surface (it configures the surface with
//! no extra view formats), so the surface itself is made plain; see [`create_pixels`] for
//! how its format is picked without ever asking for one the surface lacks.

use std::sync::Arc;

use pixels::wgpu::{self, Backends, TextureFormat};
use pixels::{Pixels, PixelsBuilder, SurfaceTexture};
use winit::window::Window;

/// A surface format and the texture format that keeps the game's bytes as they are on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Formats {
    pub surface: TextureFormat,
    pub texture: TextureFormat,
}

/// The plain pair asked for where the surface is known to offer it.
const PLAIN_BGRA: Formats = Formats {
    surface: TextureFormat::Bgra8Unorm,
    texture: TextureFormat::Rgba8Unorm,
};

/// Backends whose window surfaces always offer `Bgra8Unorm`: in wgpu-hal 29 the
/// `surface_capabilities` of the Metal, DX12 and GL adapters list it whatever the device.
/// A Vulkan surface lists what its driver reports, so Vulkan is not one of them.
const ALWAYS_BGRA8: Backends = Backends::METAL.union(Backends::DX12).union(Backends::GL);

/// The formats for a surface that supports `supported` (in the surface's order of
/// preference): the first plain 8-bit format, as egui-wgpu's own
/// `preferred_framebuffer_format` picks, with a plain RGBA texture. `None` when there is
/// no such format.
pub fn choose(supported: &[TextureFormat]) -> Option<Formats> {
    supported
        .iter()
        .copied()
        .find(|f| matches!(f, TextureFormat::Bgra8Unorm | TextureFormat::Rgba8Unorm))
        .map(|surface| Formats {
            surface,
            texture: TextureFormat::Rgba8Unorm,
        })
}

/// The backends pixels picks its adapter from: its own default (`WGPU_BACKEND`, else all),
/// as far as this build of wgpu has them (on macOS only Metal).
fn backends() -> Backends {
    Backends::from_env().unwrap_or_else(Backends::all) & wgpu::Instance::enabled_backend_features()
}

/// Whether whichever adapter pixels picks among `backends` offers [`PLAIN_BGRA`]'s surface.
fn bgra8_is_certain(backends: Backends) -> bool {
    !backends.is_empty() && ALWAYS_BGRA8.contains(backends)
}

/// Create the window's pixel buffer (`width` x `height`) on a plain surface where there
/// is one.
///
/// pixels 0.17 does not check a requested surface format: it hands it to wgpu, which
/// reports a format the surface lacks as a validation error, and that panics. Nor can the
/// format be changed once pixels is built. So a format is only asked for where it is
/// certain, and otherwise only the list of the adapter pixels itself picked is trusted:
///
/// - where every backend pixels can pick from always offers `Bgra8Unorm` (Metal, so every
///   Mac; DX12; GL), pixels is built once, asking for it;
/// - where Vulkan may be picked, pixels is built with its defaults (an sRGB surface and
///   texture), and only if the surface of the adapter it picked lists a plain 8-bit format
///   is it built again asking for that. The same request picks the same adapter: wgpu
///   lists them in a fixed order.
pub fn create_pixels(
    width: u32,
    height: u32,
    window: &Arc<Window>,
) -> Result<Pixels<'static>, pixels::Error> {
    let size = window.inner_size();
    let backends = backends();
    let builder = || {
        let surface =
            SurfaceTexture::new(size.width.max(1), size.height.max(1), Arc::clone(window));
        PixelsBuilder::new(width, height, surface).wgpu_backend(backends)
    };
    let with = |formats: Formats| {
        builder()
            .texture_format(formats.texture)
            .surface_texture_format(formats.surface)
            .build()
    };
    if bgra8_is_certain(backends) {
        return with(PLAIN_BGRA).or_else(|e| {
            log::warn!("cannot set up a plain surface ({e}); trying the default");
            builder().build()
        });
    }
    let first = builder().build()?;
    let Some(formats) = choose(&first.context().surface_capabilities.formats) else {
        log::debug!(
            "the surface has no plain 8-bit format; using {:?}",
            first.surface_texture_format()
        );
        return Ok(first);
    };
    // The first surface lets go of the window before the second is configured (Vulkan
    // allows one swapchain per window).
    drop(first);
    with(formats).or_else(|e| {
        log::warn!(
            "cannot use a {:?} surface ({e}); using the default",
            formats.surface
        );
        builder().build()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Color32, Rect, pos2};
    use std::io::Write;

    #[test]
    fn a_plain_eight_bit_surface_is_chosen_with_a_plain_texture() {
        use TextureFormat::*;
        // What Metal offers.
        assert_eq!(
            choose(&[Bgra8UnormSrgb, Bgra8Unorm, Rgba16Float, Rgb10a2Unorm]),
            Some(Formats {
                surface: Bgra8Unorm,
                texture: Rgba8Unorm
            })
        );
        // The surface's order decides, as in egui-wgpu.
        assert_eq!(
            choose(&[Rgba8UnormSrgb, Rgba8Unorm, Bgra8Unorm]).map(|f| f.surface),
            Some(Rgba8Unorm)
        );
        // Nothing plain: keep pixels' sRGB pair.
        assert_eq!(choose(&[Bgra8UnormSrgb, Rgba16Float]), None);
        assert_eq!(choose(&[]), None);
    }

    #[test]
    fn bgra8_is_asked_for_outright_only_where_every_backend_offers_it() {
        assert!(bgra8_is_certain(Backends::METAL));
        assert!(bgra8_is_certain(Backends::DX12 | Backends::GL));
        assert!(!bgra8_is_certain(Backends::VULKAN));
        assert!(!bgra8_is_certain(
            Backends::DX12 | Backends::VULKAN | Backends::GL
        ));
        assert!(!bgra8_is_certain(Backends::empty()));
        // Every Mac builds pixels once (unless WGPU_BACKEND names a backend it lacks).
        if cfg!(target_os = "macos") && std::env::var_os("WGPU_BACKEND").is_none() {
            assert_eq!(backends(), Backends::METAL);
        }
    }

    const W: u32 = 256; // 1024-byte rows, as copies to a buffer need a multiple of 256
    const H: u32 = 8;
    /// Rows `0..UI_TOP` show only the game and are checked byte for byte. egui's shapes
    /// start at row `UI_TOP + 1`; row `UI_TOP` is left between the two because their soft
    /// edge reaches half a pixel beyond them.
    const UI_TOP: u32 = 4;
    const FILL: Color32 = Color32::from_rgb(0x40, 0x80, 0xC0);
    /// A menu backdrop: black at half opacity.
    const SHADE: Color32 = Color32::from_black_alpha(0x80);

    /// The game's picture: every byte value in each channel across a row.
    fn game_pixel(x: u32, y: u32) -> [u8; 4] {
        [x as u8, ((x + 85 * y) % 256) as u8, (255 - x) as u8, 0xFF]
    }

    /// Like pixels' `scale.wgsl` at 1:1: one triangle over the target, nearest sampling.
    const PICTURE_SHADER: &str = "
@group(0) @binding(0) var picture: texture_2d<f32>;
@group(0) @binding(1) var nearest: sampler;

struct Out {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs(@builtin(vertex_index) index: u32) -> Out {
    var corners = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    let p = corners[index];
    var out: Out;
    out.position = vec4(p, 0.0, 1.0);
    out.uv = vec2(p.x + 1.0, 1.0 - p.y) * 0.5;
    return out;
}

@fragment
fn fs(in: Out) -> @location(0) vec4<f32> {
    return textureSample(picture, nearest, in.uv);
}
";

    struct Gpu {
        device: wgpu::Device,
        queue: wgpu::Queue,
    }

    fn gpu() -> Option<Gpu> {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .ok()?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()?;
        Some(Gpu { device, queue })
    }

    /// Draw the game's picture with `formats` as pixels does, then egui over the bottom rows
    /// as `Gui::draw` does (an opaque [`FILL`] on the left half, [`SHADE`] on the right), into
    /// an offscreen texture of the surface format. Returns the texels as RGBA, row by row.
    fn draw(gpu: &Gpu, formats: Formats) -> Vec<[u8; 4]> {
        let Gpu { device, queue } = gpu;
        let extent = wgpu::Extent3d {
            width: W,
            height: H,
            depth_or_array_layers: 1,
        };
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: formats.surface,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        // The game's picture.
        let picture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("picture"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: formats.texture,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let bytes: Vec<u8> = (0..H)
            .flat_map(|y| (0..W).flat_map(move |x| game_pixel(x, y)))
            .collect();
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &picture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * W),
                rows_per_image: Some(H),
            },
            extent,
        );
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("picture"),
            source: wgpu::ShaderSource::Wgsl(PICTURE_SHADER.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("picture"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                // pixels' default blend state.
                targets: &[Some(wgpu::ColorTargetState {
                    format: formats.surface,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let picture_view = picture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("picture"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&picture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        // egui, with the app's renderer options.
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_max(pos2(0.0, 0.0), pos2(W as f32, H as f32))),
            ..Default::default()
        };
        let (top, bottom, half) = ((UI_TOP + 1) as f32, H as f32, (W / 2) as f32);
        let output = ctx.run_ui(input, |ui| {
            let painter = ui.painter();
            painter.rect_filled(
                Rect::from_min_max(pos2(0.0, top), pos2(half, bottom)),
                0.0,
                FILL,
            );
            painter.rect_filled(
                Rect::from_min_max(pos2(half, top), pos2(W as f32, bottom)),
                0.0,
                SHADE,
            );
        });
        let mut renderer = egui_wgpu::Renderer::new(
            device,
            formats.surface,
            egui_wgpu::RendererOptions::default(),
        );
        for (id, delta) in &output.textures_delta.set {
            renderer.update_texture(device, queue, *id, delta);
        }
        let jobs = ctx.tessellate(output.shapes, output.pixels_per_point);
        let descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [W, H],
            pixels_per_point: output.pixels_per_point,
        };

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        drop(renderer.update_buffers(device, queue, &mut encoder, &jobs, &descriptor));
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("picture"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("ui"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &target_view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
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
        }

        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(4 * W * H),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * W),
                    rows_per_image: Some(H),
                },
            },
            extent,
        );
        queue.submit(Some(encoder.finish()));
        readback.map_async(wgpu::MapMode::Read, .., |r| r.expect("map the readback"));
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("wait for the GPU");
        let bgra = matches!(
            formats.surface,
            TextureFormat::Bgra8Unorm | TextureFormat::Bgra8UnormSrgb
        );
        let texels = readback
            .get_mapped_range(..)
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&[c0, c1, c2, alpha]| {
                if bgra {
                    [c2, c1, c0, alpha]
                } else {
                    [c0, c1, c2, alpha]
                }
            })
            .collect();
        readback.unmap();
        texels
    }

    fn at(texels: &[[u8; 4]], x: u32, y: u32) -> [u8; 4] {
        texels[(y * W + x) as usize]
    }

    fn near(got: [u8; 4], want: [u8; 3], tolerance: u8) -> bool {
        got[..3]
            .iter()
            .zip(want)
            .all(|(&g, w)| g.abs_diff(w) <= tolerance)
    }

    fn linear(v: f64) -> f64 {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }

    fn encoded(v: f64) -> f64 {
        if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    }

    /// The game rows of `texels` hold exactly the bytes the game wrote.
    /// The texels of the game rows that differ from what the game wrote: (x, y, got, wrote).
    fn game_rows_changed(texels: &[[u8; 4]]) -> Vec<(u32, u32, [u8; 4], [u8; 4])> {
        (0..UI_TOP)
            .flat_map(|y| (0..W).map(move |x| (x, y)))
            .map(|(x, y)| (x, y, at(texels, x, y), game_pixel(x, y)))
            .filter(|(_, _, got, wrote)| got[..3] != wrote[..3])
            .collect()
    }

    #[test]
    fn the_game_keeps_its_bytes_and_egui_blends_as_egui_means_to() {
        let Some(gpu) = gpu() else {
            // Straight to stderr: the test harness would swallow `eprintln!` from a pass.
            let _ = writeln!(
                std::io::stderr(),
                "the_game_keeps_its_bytes_and_egui_blends_as_egui_means_to: skipped, no GPU \
                 adapter here (the format choice is still tested)"
            );
            return;
        };
        let today = Formats {
            surface: TextureFormat::Bgra8UnormSrgb,
            texture: TextureFormat::Rgba8UnormSrgb,
        };
        // What the app picks on Metal's list of formats.
        let chosen = choose(&[
            TextureFormat::Bgra8UnormSrgb,
            TextureFormat::Bgra8Unorm,
            TextureFormat::Rgba16Float,
        ])
        .unwrap();
        assert_eq!(chosen.surface, TextureFormat::Bgra8Unorm);
        let old = draw(&gpu, today);
        let new = draw(&gpu, chosen);

        // The game's picture: the same bytes as written, before and after, for every value.
        let (old_changed, new_changed) = (game_rows_changed(&old), game_rows_changed(&new));
        assert!(
            old_changed.is_empty(),
            "the sRGB pair round-trips: {old_changed:?}"
        );
        assert!(
            new_changed.is_empty(),
            "the plain pair copies: {new_changed:?}"
        );

        // A solid egui colour comes out as stored on both (egui-wgpu compensates on sRGB).
        let (fill_x, shade_x, y) = (W / 4, W - W / 4, (UI_TOP + 1 + H) / 2);
        let fill = [FILL.r(), FILL.g(), FILL.b()];
        assert!(
            near(at(&new, fill_x, y), fill, 1),
            "{:?}",
            at(&new, fill_x, y)
        );
        assert!(
            near(at(&old, fill_x, y), fill, 1),
            "{:?}",
            at(&old, fill_x, y)
        );

        // Translucency is where they differ. On the plain surface egui's backdrop darkens
        // the game as egui computes it, in gamma space; on the sRGB one the GPU blends in
        // linear light and the result is much lighter.
        let keep = 1.0 - f64::from(SHADE.a()) / 255.0;
        let under = game_pixel(shade_x, y);
        let gamma: [u8; 3] = std::array::from_fn(|c| (f64::from(under[c]) * keep).round() as u8);
        let lin: [u8; 3] = std::array::from_fn(|c| {
            (encoded(linear(f64::from(under[c]) / 255.0) * keep) * 255.0).round() as u8
        });
        let got_new = at(&new, shade_x, y);
        let got_old = at(&old, shade_x, y);
        assert!(near(got_new, gamma, 1), "{got_new:?} vs {gamma:?}");
        assert!(near(got_old, lin, 2), "{got_old:?} vs {lin:?}");
        assert!(
            got_old[0] >= got_new[0] + 20,
            "the sRGB surface blends lighter: {got_old:?} vs {got_new:?}"
        );

        // The two formats change together: an sRGB texture on the plain surface darkens
        // the game, which this test would see.
        let mismatched = draw(
            &gpu,
            Formats {
                surface: TextureFormat::Bgra8Unorm,
                texture: TextureFormat::Rgba8UnormSrgb,
            },
        );
        assert!(!game_rows_changed(&mismatched).is_empty());
        assert!(at(&mismatched, 0x88, 0)[0] < 0x88 - 20);
    }
}
