//! The GPU side of the overlay: device, pipelines, glyph atlas and the per-open surface.
//! `Gfx` outlives every open; only `Target` is created and dropped on each toggle.

use anyhow::{anyhow, bail, Context, Result};
use bytemuck::{Pod, Zeroable};
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Wrap,
};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use wgpu::util::DeviceExt;
use winit::window::Window;

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;
const MAX_TEXTURE: u32 = 2048;
const MAX_CACHED_TEXTS: usize = 400;

/// One rounded rectangle in physical pixels. Colors are linear RGBA.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Quad {
    pub pos: [f32; 2],
    pub size: [f32; 2],
    pub color: [f32; 4],
    pub border_color: [f32; 4],
    pub radius: f32,
    pub border: f32,
    pub pad: [f32; 2],
}

pub struct Text {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub size: f32,
    pub color: [u8; 4],
    /// Wraps at this width; `None` keeps one line.
    pub wrap: Option<f32>,
    pub mono: bool,
    /// Left, top, right, bottom in physical pixels.
    pub clip: [f32; 4],
}

pub struct Tex {
    bind: wgpu::BindGroup,
}

#[derive(Default)]
pub struct Scene {
    pub quads: Vec<Quad>,
    pub images: Vec<(Quad, Rc<Tex>)>,
    pub texts: Vec<Text>,
}

pub struct Target {
    pub window: Arc<dyn Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
}

type TextKey = (String, u32, u32, bool);

pub struct Gfx {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    screen_buf: wgpu::Buffer,
    screen_bind: wgpu::BindGroup,
    tex_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    rect_pipe: wgpu::RenderPipeline,
    image_pipe: wgpu::RenderPipeline,
    fonts: FontSystem,
    swash: SwashCache,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    viewport: Viewport,
    texts: HashMap<TextKey, Buffer>,
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// `0xRRGGBB` in sRGB to linear RGBA.
pub fn lin(hex: u32, a: f32) -> [f32; 4] {
    let ch = |shift: u32| srgb_to_linear(((hex >> shift) & 0xff) as f32 / 255.0);
    [ch(16), ch(8), ch(0), a]
}

pub fn rgba8(hex: u32, a: f32) -> [u8; 4] {
    [
        (hex >> 16) as u8,
        (hex >> 8) as u8,
        hex as u8,
        (a * 255.0) as u8,
    ]
}

impl Gfx {
    pub fn new() -> Result<Gfx> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .context("no GPU adapter")?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: None,
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .context("GPU device")?;

        let screen_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let screen_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let screen_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &screen_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: screen_buf.as_entire_binding(),
            }],
        });
        let tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(include_str!("quad.wgsl").into()),
        });
        let pipeline = |layouts: &[&wgpu::BindGroupLayout], fragment: &str| {
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: layouts,
                push_constant_ranges: &[],
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: None,
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<Quad>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &wgpu::vertex_attr_array![
                            0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Float32x4, 4 => Float32x2
                        ],
                    }],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(fragment),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FORMAT,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };
        let rect_pipe = pipeline(&[&screen_layout], "fs_rect");
        let image_pipe = pipeline(&[&screen_layout, &tex_layout], "fs_image");

        // Parses all system fonts; do it once.
        let fonts = FontSystem::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, FORMAT);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);

        Ok(Gfx {
            instance,
            adapter,
            device,
            queue,
            screen_buf,
            screen_bind,
            tex_layout,
            sampler,
            rect_pipe,
            image_pipe,
            fonts,
            swash: SwashCache::new(),
            atlas,
            text_renderer,
            viewport,
            texts: HashMap::new(),
        })
    }

    pub fn target(&self, window: Arc<dyn Window>) -> Result<Target> {
        let surface = self
            .instance
            .create_surface(window.clone())
            .context("GPU surface")?;
        let size = window.surface_size();
        let caps = surface.get_capabilities(&self.adapter);
        if !caps.formats.contains(&FORMAT) {
            bail!("surface lacks {FORMAT:?}");
        }
        let alpha_mode = [
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::PostMultiplied,
        ]
        .into_iter()
        .find(|m| caps.alpha_modes.contains(m))
        .unwrap_or(caps.alpha_modes[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: FORMAT,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&self.device, &config);
        Ok(Target {
            window,
            surface,
            config,
        })
    }

    pub fn upload(&self, img: &image::RgbaImage) -> Rc<Tex> {
        let small;
        let img = if img.width() > MAX_TEXTURE || img.height() > MAX_TEXTURE {
            small = image::imageops::thumbnail(img, MAX_TEXTURE, MAX_TEXTURE);
            &small
        } else {
            img
        };
        let texture = self.device.create_texture_with_data(
            &self.queue,
            &wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width: img.width(),
                    height: img.height(),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            img.as_raw(),
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.tex_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        Rc::new(Tex { bind })
    }

    fn shape(&mut self, t: &Text) -> TextKey {
        let key = (
            t.text.clone(),
            t.size.to_bits(),
            t.wrap.map_or(0, f32::to_bits),
            t.mono,
        );
        if !self.texts.contains_key(&key) {
            let mut buf = Buffer::new(
                &mut self.fonts,
                Metrics::new(t.size, (t.size * 1.25).round()),
            );
            buf.set_wrap(
                &mut self.fonts,
                if t.wrap.is_some() {
                    Wrap::Word
                } else {
                    Wrap::None
                },
            );
            buf.set_size(&mut self.fonts, t.wrap, None);
            let family = if t.mono {
                Family::Monospace
            } else {
                Family::SansSerif
            };
            buf.set_text(
                &mut self.fonts,
                &t.text,
                &Attrs::new().family(family),
                Shaping::Advanced,
            );
            buf.shape_until_scroll(&mut self.fonts, false);
            self.texts.insert(key.clone(), buf);
        }
        key
    }

    /// Width in physical pixels of `text` on one line.
    pub fn text_width(&mut self, text: &str, size: f32, mono: bool) -> f32 {
        let t = Text {
            text: text.into(),
            x: 0.0,
            y: 0.0,
            size,
            color: [0; 4],
            wrap: None,
            mono,
            clip: [0.0; 4],
        };
        let key = self.shape(&t);
        self.texts[&key]
            .layout_runs()
            .map(|r| r.line_w)
            .fold(0.0, f32::max)
    }

    /// Draws the scene. Returns false when the surface needs another try.
    pub fn render(&mut self, target: &mut Target, scene: &Scene) -> Result<bool> {
        let size = target.window.surface_size();
        let (w, h) = (size.width.max(1), size.height.max(1));
        if (w, h) != (target.config.width, target.config.height) {
            target.config.width = w;
            target.config.height = h;
            target.surface.configure(&self.device, &target.config);
        }
        let frame = match target.surface.get_current_texture() {
            Ok(f) => f,
            Err(
                wgpu::SurfaceError::Outdated
                | wgpu::SurfaceError::Lost
                | wgpu::SurfaceError::Timeout,
            ) => {
                target.surface.configure(&self.device, &target.config);
                return Ok(false);
            }
            Err(e) => bail!("surface: {e}"),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        self.queue.write_buffer(
            &self.screen_buf,
            0,
            bytemuck::cast_slice(&[w as f32, h as f32, 0.0, 0.0]),
        );
        let instances: Vec<Quad> = scene
            .quads
            .iter()
            .copied()
            .chain(scene.images.iter().map(|(q, _)| *q))
            .collect();
        let instance_buf = (!instances.is_empty()).then(|| {
            self.device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: None,
                    contents: bytemuck::cast_slice(&instances),
                    usage: wgpu::BufferUsages::VERTEX,
                })
        });

        if self.texts.len() > MAX_CACHED_TEXTS {
            self.texts.clear();
        }
        let keys: Vec<TextKey> = scene.texts.iter().map(|t| self.shape(t)).collect();
        self.viewport.update(
            &self.queue,
            Resolution {
                width: w,
                height: h,
            },
        );
        let areas = scene.texts.iter().zip(&keys).map(|(t, key)| TextArea {
            buffer: &self.texts[key],
            left: t.x,
            top: t.y,
            scale: 1.0,
            bounds: TextBounds {
                left: t.clip[0] as i32,
                top: t.clip[1] as i32,
                right: t.clip[2] as i32,
                bottom: t.clip[3] as i32,
            },
            default_color: Color::rgba(t.color[0], t.color[1], t.color[2], t.color[3]),
            custom_glyphs: &[],
        });
        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.fonts,
                &mut self.atlas,
                &self.viewport,
                areas,
                &mut self.swash,
            )
            .map_err(|e| anyhow!("text prepare: {e}"))?;

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if let Some(buf) = &instance_buf {
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.set_bind_group(0, &self.screen_bind, &[]);
                pass.set_pipeline(&self.rect_pipe);
                pass.draw(0..4, 0..scene.quads.len() as u32);
                pass.set_pipeline(&self.image_pipe);
                for (i, (_, tex)) in scene.images.iter().enumerate() {
                    let at = (scene.quads.len() + i) as u32;
                    pass.set_bind_group(1, &tex.bind, &[]);
                    pass.draw(0..4, at..at + 1);
                }
            }
            let _ = self
                .text_renderer
                .render(&self.atlas, &self.viewport, &mut pass);
        }
        self.queue.submit([encoder.finish()]);
        target.window.pre_present_notify();
        frame.present();
        self.atlas.trim();
        Ok(true)
    }
}
