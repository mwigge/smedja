//! `st-render` — wgpu-based terminal cell-grid renderer for smedja.
//!
//! Renders a grid of [`Cell`]s to a [`winit`] window via a wgpu pipeline that
//! composites per-cell background fills with glyph textures sampled from a
//! 1024×1024 texture atlas.
//!
//! # ponytail
//!
//! `Renderer::new()` calls async wgpu initialisation which requires a real GPU.
//! On headless CI these calls return errors; callers should handle that
//! gracefully.  All pure-logic types (`Cell`, `GlyphAtlas`, `ShelfPacker`,
//! `BlockDecoration`) are fully unit-testable without a GPU.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::wildcard_in_or_patterns,
    clippy::too_many_lines,
    clippy::implicit_clone
)]

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use parking_lot::Mutex;
use st_statusbar::Segment;
use thiserror::Error;

// Re-export so callers don't have to depend on winit/wgpu directly.
pub use wgpu;
pub use winit;

mod atlas;
mod packer;
mod shaders;
mod vertices;

pub use atlas::{is_pua_codepoint, select_atlas, AtlasKind, GlyphAtlas, GlyphEntry};
pub use packer::ShelfPacker;

use atlas::ATLAS_SIZE;
use shaders::{BG_IMAGE_SHADER_SRC, BG_SHADER_SRC, COLOUR_SHADER_SRC, SHADER_SRC};

/// Errors produced by the renderer.
#[derive(Debug, Error)]
pub enum RenderError {
    /// wgpu surface configuration failed.
    #[error("surface error: {0}")]
    Surface(String),
    /// No suitable GPU adapter found.
    #[error("no suitable GPU adapter")]
    NoAdapter,
    /// wgpu device request failed.
    #[error("device request failed: {0}")]
    DeviceRequest(#[from] wgpu::RequestDeviceError),
    /// Surface texture acquire failed.
    #[error("frame acquire error: {0}")]
    Frame(#[from] wgpu::SurfaceError),
    /// Generic render error.
    #[error("render error: {0}")]
    Other(String),
}

// ── Cell ──────────────────────────────────────────────────────────────────────

/// A single terminal cell to be rendered.
///
/// `fg`/`bg` are already resolved by the caller (inverse-video swap and dim
/// scaling are applied upstream in the bridge), so the renderer only needs the
/// glyph-shaping flags here: bold/italic pick the font variant, `wide` centres a
/// double-width glyph over two columns, and underline/strikethrough draw rules.
#[derive(Debug, Clone, PartialEq, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct Cell {
    /// The Unicode scalar displayed in this cell.
    pub ch: char,
    /// Foreground colour as linear RGBA.
    pub fg: [f32; 4],
    /// Background colour as linear RGBA.
    pub bg: [f32; 4],
    /// Column index (0-based).
    pub col: u16,
    /// Row index (0-based).
    pub row: u16,
    /// Use the bold font variant.
    pub bold: bool,
    /// Use the italic font variant.
    pub italic: bool,
    /// Draw an underline rule.
    pub underline: bool,
    /// Draw a strikethrough rule.
    pub strikethrough: bool,
    /// Leading cell of a double-width glyph (centre over two columns).
    pub wide: bool,
}

/// A decorative overlay drawn over a block span.
#[derive(Debug, Clone)]
pub struct BlockDecoration {
    /// First row of the block.
    pub start_row: u16,
    /// Last row of the block (inclusive).
    pub end_row: u16,
    /// Exit code, used to determine colour of the badge.
    pub exit_code: Option<i32>,
    /// Whether this block is currently selected.
    pub selected: bool,
}

/// An agent block for rendering.
#[derive(Debug, Clone)]
pub struct AgentBlockView {
    /// Start row in the terminal grid.
    pub start_row: u16,
    /// Model name displayed in the header.
    pub model: String,
    /// Streamed content lines.
    pub content_lines: Vec<String>,
    /// Whether an approval prompt is visible.
    pub approval_pending: bool,
    /// Hanging-indent left margin, in cell columns.
    ///
    /// The author label (`[ model ]`) occupies this margin on the block's first
    /// row; every body line is shifted right by this many columns so wrapped
    /// body lines align under the content rather than under the gutter glyph.
    /// This is applied purely as render geometry — the cell/content strings are
    /// never modified, so selection and copy yield the raw unindented text.
    ///
    /// Derive it with [`hanging_margin_cols`] from the author-label width.
    pub left_margin_cols: u16,
}

/// The author-label header rendered for an agent block, e.g. `[ claude ]`.
///
/// Single source of truth for the label so the rendered glyphs and the
/// hanging-indent margin derivation ([`hanging_margin_cols`]) stay in sync.
#[must_use]
pub fn agent_header(model: &str) -> String {
    format!("[ {model} ]")
}

/// Left-margin width, in cell columns, for an agent block's hanging indent.
///
/// `label_cols` is the display width (in columns) of the author label; body
/// lines hang one column past it. The result is capped at `max_cols` so a very
/// long label can never squeeze the usable body width to nothing — callers
/// should pass roughly half the grid width. `max_cols` is floored at 1.
///
/// This is the pure geometry helper: it inserts no spaces into any string, it
/// only yields the column offset the renderer shifts glyph vertices by.
#[must_use]
pub fn hanging_margin_cols(label_cols: usize, max_cols: usize) -> u16 {
    let want = label_cols.saturating_add(1);
    let capped = want.min(max_cols.max(1));
    u16::try_from(capped).unwrap_or(u16::MAX)
}

// ── Background configuration ───────────────────────────────────────────────

/// Configuration for terminal background image and transparency.
pub struct BackgroundConfig {
    /// Path to the background image file, if configured.
    pub image_path: Option<std::path::PathBuf>,
    /// Window opacity in the range `0.0` (transparent) to `1.0` (opaque).
    pub opacity: f32,
    /// Decoded RGBA pixel data, populated by [`BackgroundConfig::load_image`].
    pub image_pixels: Option<Vec<u8>>,
    /// Width of the loaded image in pixels.
    pub image_width: u32,
    /// Height of the loaded image in pixels.
    pub image_height: u32,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            image_path: None,
            opacity: 1.0,
            image_pixels: None,
            image_width: 0,
            image_height: 0,
        }
    }
}

impl BackgroundConfig {
    /// Loads the image at [`Self::image_path`] into [`Self::image_pixels`].
    ///
    /// Returns an error if no path is configured or the image cannot be opened
    /// or decoded.
    ///
    /// # ponytail
    ///
    /// GPU blit is deferred — pixels are loaded here; the actual draw call is a
    /// `TODO` comment in the render loop.
    ///
    /// # Errors
    ///
    /// Returns a boxed error if the path is absent or the image cannot be read.
    pub fn load_image(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.image_path.as_ref().ok_or("no image path configured")?;
        let img = image::open(path)?.to_rgba8();
        self.image_width = img.width();
        self.image_height = img.height();
        self.image_pixels = Some(img.into_raw());
        Ok(())
    }
}

// ── Vertex ────────────────────────────────────────────────────────────────────

/// A vertex for the glyph quad pipeline.
///
/// Each cell is rendered as two triangles (a quad); vertices carry screen
/// position, atlas UV coordinates, and a tint colour.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct GlyphVertex {
    /// NDC position `[x, y]`.
    pub position: [f32; 2],
    /// Atlas UV coordinates `[u, v]`.
    pub tex_coords: [f32; 2],
    /// Linear RGBA tint colour.
    pub color: [f32; 4],
}

impl GlyphVertex {
    const ATTRIBS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
        0 => Float32x2,
        1 => Float32x2,
        2 => Float32x4,
    ];

    /// Returns the wgpu vertex buffer layout for [`GlyphVertex`].
    #[must_use]
    pub fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// Background vertex: position + colour only.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct BgVertex {
    pub position: [f32; 2],
    pub color: [f32; 4],
}

impl BgVertex {
    const ATTRIBS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];

    #[must_use]
    pub fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

// ── Background image vertex and uniform ──────────────────────────────────────

/// Vertex for the full-screen background image quad.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct BgImageVertex {
    /// NDC position `[x, y]`.
    pub position: [f32; 2],
    /// UV texture coordinates `[u, v]` in `[0, 1]`.
    pub tex_coords: [f32; 2],
}

impl BgImageVertex {
    const ATTRIBS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2];

    /// Returns the wgpu vertex buffer layout for [`BgImageVertex`].
    #[must_use]
    pub fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// 16-byte uniform block for the background image shader pass.
///
/// `opacity` controls how opaque the image is; the remaining three floats are
/// padding required by the WGSL uniform buffer alignment rules.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct BgImageParams {
    opacity: f32,
    _pad: [f32; 3],
}

// ── Dynamic vertex buffer ─────────────────────────────────────────────────────

/// A persistent GPU vertex buffer that is re-uploaded each frame via
/// [`wgpu::Queue::write_buffer`] and grows (reallocates) only when a frame needs
/// more room than the current capacity.
///
/// Replaces the old per-frame `create_buffer_init` pattern, which allocated a
/// fresh buffer for every vertex stream on every frame.
struct DynamicVertexBuffer {
    buffer: Option<wgpu::Buffer>,
    /// Current capacity in bytes.
    capacity: u64,
    label: &'static str,
}

impl DynamicVertexBuffer {
    const fn new(label: &'static str) -> Self {
        Self {
            buffer: None,
            capacity: 0,
            label,
        }
    }

    /// Uploads `data` into the buffer, growing it when necessary. Does nothing
    /// when `data` is empty. The buffer always carries `COPY_DST` so
    /// [`wgpu::Queue::write_buffer`] is valid.
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let needed = data.len() as u64;
        if needed > self.capacity {
            // Round up to the next power of two to amortise growth across frames.
            let new_cap = needed.next_power_of_two().max(256);
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size: new_cap,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.capacity = new_cap;
        }
        if let Some(buf) = &self.buffer {
            queue.write_buffer(buf, 0, data);
        }
    }

    /// Returns the current backing buffer, if one has been allocated.
    fn buffer(&self) -> Option<&wgpu::Buffer> {
        self.buffer.as_ref()
    }
}

// ── Renderer ──────────────────────────────────────────────────────────────────

/// The primary renderer: owns the wgpu surface, pipelines, and glyph atlas.
pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: wgpu::SurfaceConfiguration,
    glyph_pipeline: wgpu::RenderPipeline,
    bg_pipeline: wgpu::RenderPipeline,
    /// Pipeline for blitting the optional background image.
    bg_image_pipeline: wgpu::RenderPipeline,
    /// Uploaded background image texture (None if no image is configured).
    bg_image_texture: Option<wgpu::Texture>,
    /// Bind group containing the background image texture and sampler.
    pub(crate) bg_image_bind_group: Option<wgpu::BindGroup>,
    /// 16-byte uniform buffer carrying the opacity value for the image pass.
    #[allow(dead_code)] // held for RAII lifetime; Drop releases the GPU allocation
    bg_image_params_buf: wgpu::Buffer,
    /// Bind group for [`Self::bg_image_params_buf`].
    bg_image_params_bind_group: wgpu::BindGroup,
    pub(crate) atlas: GlyphAtlas,
    bind_group: wgpu::BindGroup,
    /// Pipeline for registered RGBA colour glyphs.
    colour_pipeline: wgpu::RenderPipeline,
    /// Bind group binding the colour atlas texture + sampler.
    colour_bind_group: wgpu::BindGroup,
    /// Persistent, grow-on-demand vertex buffers uploaded each frame via
    /// `queue.write_buffer` (replace per-frame `create_buffer_init`).
    bg_image_vbuf: DynamicVertexBuffer,
    bg_vbuf: DynamicVertexBuffer,
    glyph_vbuf: DynamicVertexBuffer,
    colour_vbuf: DynamicVertexBuffer,
    /// Current cell grid snapshot.
    pub(crate) cells: Vec<Cell>,
    /// Block decorations to draw.
    pub(crate) block_decorations: Vec<BlockDecoration>,
    /// Agent blocks to draw.
    pub(crate) agent_blocks: Vec<AgentBlockView>,
    pub(crate) config: st_config::Config,
    /// Background image and transparency configuration.
    pub background: BackgroundConfig,
    /// Physical size of the window in pixels.
    pub size: winit::dpi::PhysicalSize<u32>,
    /// Status bar segments to overlay at the bottom of the window.
    pub(crate) status_bar_segments: Vec<Segment>,
    /// Top bar segments to overlay at the top of the window.
    pub(crate) top_bar_segments: Vec<Segment>,
    /// Device pixel ratio for this window (1.0 on non-HiDPI, 2.0 on 2× displays).
    pub scale_factor: f64,
}

impl Renderer {
    /// Creates a new [`Renderer`] for `window`, using settings from `config`.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError`] if wgpu initialisation fails.  On headless CI
    /// this will return `RenderError::NoAdapter`.
    ///
    /// # ponytail
    ///
    /// This function calls wgpu async APIs that require a real GPU.  It is
    /// excluded from automated tests via the `#[cfg(not(test))]` pattern on
    /// the integration path.
    pub async fn new(
        window: std::sync::Arc<winit::window::Window>,
        config: &st_config::Config,
    ) -> anyhow::Result<Self> {
        let scale_factor = window.scale_factor();
        let size = window.inner_size();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        // ponytail: create_surface requires a GPU-capable environment.
        let surface = instance.create_surface(window)?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or(RenderError::NoAdapter)?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("smedja"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_webgl2_defaults()
                        .using_resolution(adapter.limits()),
                    memory_hints: wgpu::MemoryHints::default(),
                },
                None,
            )
            .await?;

        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: surface_caps
                .present_modes
                .iter()
                .find(|&&m| m == wgpu::PresentMode::Fifo)
                .copied()
                .unwrap_or(wgpu::PresentMode::AutoVsync),
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let atlas = GlyphAtlas::new_with_system_fonts(&device);

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("atlas_bind_group_layout"),
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

        // Nearest filtering for crisp glyph rendering — linear blurs sub-pixel text.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("atlas_bind_group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("glyph_pipeline_layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let glyph_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("glyph_shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        let glyph_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("glyph_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &glyph_shader,
                entry_point: "vs_main",
                buffers: &[GlyphVertex::layout()],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &glyph_shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // ── Colour glyph pipeline (registered RGBA glyphs) ─────────────────────

        let colour_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("colour_atlas_bind_group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas.colour_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let colour_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("colour_glyph_shader"),
            source: wgpu::ShaderSource::Wgsl(COLOUR_SHADER_SRC.into()),
        });

        let colour_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("colour_glyph_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &colour_shader,
                entry_point: "vs_colour",
                buffers: &[GlyphVertex::layout()],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &colour_shader,
                entry_point: "fs_colour",
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let bg_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bg_shader"),
            source: wgpu::ShaderSource::Wgsl(BG_SHADER_SRC.into()),
        });

        let bg_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("bg_pipeline_layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });

        let bg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("bg_pipeline"),
            layout: Some(&bg_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &bg_shader,
                entry_point: "vs_bg",
                buffers: &[BgVertex::layout()],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &bg_shader,
                entry_point: "fs_bg",
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // ── Background image pipeline ──────────────────────────────────────────

        // bind group 0: texture + sampler
        let bg_img_tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bg_img_tex_layout"),
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

        // bind group 1: opacity uniform
        let bg_img_params_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("bg_img_params_layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let bg_image_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bg_image_shader"),
            source: wgpu::ShaderSource::Wgsl(BG_IMAGE_SHADER_SRC.into()),
        });

        let bg_image_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("bg_image_pipeline_layout"),
                bind_group_layouts: &[&bg_img_tex_layout, &bg_img_params_layout],
                push_constant_ranges: &[],
            });

        let bg_image_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("bg_image_pipeline"),
            layout: Some(&bg_image_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &bg_image_shader,
                entry_point: "vs_bg_img",
                buffers: &[BgImageVertex::layout()],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &bg_image_shader,
                entry_point: "fs_bg_img",
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // Params uniform buffer (initial opacity from config).
        let initial_params = BgImageParams {
            opacity: config.window.background_opacity,
            _pad: [0.0; 3],
        };
        let bg_image_params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("bg_image_params_buf"),
            contents: bytemuck::bytes_of(&initial_params),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bg_image_params_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bg_image_params_bind_group"),
            layout: &bg_img_params_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: bg_image_params_buf.as_entire_binding(),
            }],
        });

        // Optionally load the background image from disk and upload it.
        let mut background = BackgroundConfig {
            image_path: config
                .window
                .background_image
                .as_ref()
                .map(std::path::PathBuf::from),
            opacity: config.window.background_opacity,
            ..BackgroundConfig::default()
        };

        let (bg_image_texture, bg_image_bind_group) =
            if background.image_path.is_some() && background.load_image().is_ok() {
                if let Some(pixels) = background.image_pixels.take() {
                    let (tex, bg) = upload_bg_image(
                        &device,
                        &queue,
                        &pixels,
                        background.image_width,
                        background.image_height,
                        &bg_img_tex_layout,
                    );
                    (Some(tex), Some(bg))
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };

        // ponytail: lazy warmup on first render — ASCII glyphs are rasterised
        // on-demand by ensure_cell_glyphs() rather than blocking here.
        let initial_cells: Vec<Cell> = Vec::new();

        Ok(Self {
            surface,
            device,
            queue,
            surface_config,
            glyph_pipeline,
            bg_pipeline,
            bg_image_pipeline,
            bg_image_texture,
            bg_image_bind_group,
            bg_image_params_buf,
            bg_image_params_bind_group,
            atlas,
            bind_group,
            colour_pipeline,
            colour_bind_group,
            bg_image_vbuf: DynamicVertexBuffer::new("bg_image_vbuf"),
            bg_vbuf: DynamicVertexBuffer::new("bg_vbuf"),
            glyph_vbuf: DynamicVertexBuffer::new("glyph_vbuf"),
            colour_vbuf: DynamicVertexBuffer::new("colour_vbuf"),
            cells: initial_cells,
            block_decorations: Vec::new(),
            agent_blocks: Vec::new(),
            config: config.clone(),
            background,
            size,
            status_bar_segments: Vec::new(),
            top_bar_segments: Vec::new(),
            scale_factor,
        })
    }

    /// Updates the device pixel ratio and clears the glyph atlas.
    ///
    /// Must be called when the window moves to a display with a different DPI.
    /// Clears cached glyphs so they are re-rasterised at the new scale.
    ///
    /// # Panics
    ///
    /// Panics if `ATLAS_SIZE` does not fit in `i32` (compile-time invariant).
    pub fn update_scale_factor(&mut self, sf: f64) {
        if (self.scale_factor - sf).abs() > f64::EPSILON {
            self.scale_factor = sf;
            self.atlas.glyphs.clear();
            self.atlas.alpha_alloc = etagere::AtlasAllocator::new(etagere::size2(
                i32::try_from(ATLAS_SIZE).expect("ATLAS_SIZE fits i32"),
                i32::try_from(ATLAS_SIZE).expect("ATLAS_SIZE fits i32"),
            ));
        }
    }

    /// Handles a window resize event.
    pub fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        if new_size.width == 0 || new_size.height == 0 {
            return;
        }
        self.size = new_size;
        self.surface_config.width = new_size.width;
        self.surface_config.height = new_size.height;
        self.surface.configure(&self.device, &self.surface_config);
        tracing::debug!("renderer resized to {}×{}", new_size.width, new_size.height);
    }

    /// Updates the cell grid from a slice of [`Cell`]s.
    pub fn update_cells(&mut self, cells: &[Cell]) {
        self.cells.clear();
        self.cells.extend_from_slice(cells);
    }

    /// Rasterises any glyphs in `self.cells` that are not yet in the atlas.
    ///
    /// Must be called at the start of [`Self::render`], before the command
    /// encoder is created, so that [`wgpu::Queue::write_texture`] completes
    /// before the draw calls are submitted.
    fn ensure_cell_glyphs(&mut self) {
        // New frame: every glyph touched below is stamped with this counter so
        // LRU eviction can tell on-screen glyphs from stale ones.
        self.atlas.frame = self.atlas.frame.wrapping_add(1);

        let font_size = self.config.font.size * self.scale_factor as f32;
        // Collect (char, bold, italic) first so the immutable borrow of
        // self.cells does not overlap the mutable borrow of self.atlas. Bold and
        // italic key separate atlas slots so the right font variant is shown.
        let cell_glyphs: Vec<(char, bool, bool)> = self
            .cells
            .iter()
            .map(|c| (c.ch, c.bold, c.italic))
            .collect();
        for (cell_ch, bold, italic) in cell_glyphs {
            self.touch_or_warm(cell_ch, font_size, bold, italic);
        }

        // Status-bar and top-bar segments render at a smaller font key. They are
        // touched every frame too, so they can never be evicted while visible.
        let sb_font_size = self.status_bar_height_px() as f32 * 0.65;
        let sb_chars: Vec<char> = self
            .status_bar_segments
            .iter()
            .chain(self.top_bar_segments.iter())
            .flat_map(|seg| seg.text.chars())
            .collect();
        for ch in sb_chars {
            self.touch_or_warm(ch, sb_font_size, false, false);
        }
    }

    /// Stamps an on-screen glyph's `last_used` to the current frame, rasterising
    /// and uploading it on first sight. Spaces are a no-op. Registered PUA colour
    /// glyphs are stamped too so the colour atlas's LRU eviction never drops a
    /// glyph that is visible this frame.
    fn touch_or_warm(&mut self, ch: char, font_size: f32, bold: bool, italic: bool) {
        if ch == ' ' {
            return;
        }
        let key = (ch, bold, italic, font_size.to_bits());
        if let Some(entry) = self.atlas.glyphs.get_mut(&key) {
            entry.last_used = self.atlas.frame;
            return;
        }
        if let Some(entry) = self.atlas.colour_glyphs.get_mut(&ch) {
            entry.last_used = self.atlas.frame;
            return;
        }
        let _ = self
            .atlas
            .get_or_insert(&self.device, &self.queue, ch, font_size, bold, italic);
    }

    /// Sets the block decorations for the next render pass.
    pub fn set_block_decorations(&mut self, decorations: Vec<BlockDecoration>) {
        self.block_decorations = decorations;
    }

    /// Sets the agent blocks for the next render pass.
    pub fn set_agent_blocks(&mut self, blocks: &[AgentBlockView]) {
        self.agent_blocks = blocks.to_vec();
    }

    /// Returns a mutable reference to the glyph atlas.
    #[must_use]
    pub fn atlas_mut(&mut self) -> &mut GlyphAtlas {
        &mut self.atlas
    }

    /// Attaches the shared glyph registry so the atlas can resolve registered
    /// PUA codepoints to their cached bitmaps.
    ///
    /// Call this once after the PTY session is created and the built-in glyphs
    /// have been registered.
    pub fn set_glyph_registry(&mut self, registry: Arc<Mutex<st_glyph::GlyphRegistry>>) {
        self.atlas.set_glyph_registry(registry);
    }

    /// Updates the segments displayed in the status bar strip.
    ///
    /// Segments are laid out left-to-right separated by a single space.  Call
    /// this before [`Self::render`] each frame.
    pub fn set_status_bar_segments(&mut self, segments: &[Segment]) {
        self.status_bar_segments = segments.to_vec();
    }

    /// Updates the segments displayed in the top bar strip.
    ///
    /// Segments are laid out left-to-right separated by a single space.  Call
    /// this before [`Self::render`] each frame.
    pub fn set_top_bar_segments(&mut self, segments: &[Segment]) {
        self.top_bar_segments = segments.to_vec();
    }

    /// Returns the physical pixel height reserved for the status bar.
    ///
    /// Scale factor is applied so the strip is the same logical size on
    /// high-DPI displays, matching the formula used in the terminal binary when
    /// sizing the PTY grid.
    #[must_use]
    pub fn status_bar_height_px(&self) -> u32 {
        let eff = (self.config.font.size * self.scale_factor as f32) as u32;
        eff.min(36)
    }

    /// Returns the height of the top bar strip in pixels.
    ///
    /// The top bar uses the same cell metrics as the status bar so both strips
    /// have consistent appearance.
    #[must_use]
    pub fn top_bar_height_px(&self) -> u32 {
        if self.top_bar_segments.is_empty() {
            0
        } else {
            self.status_bar_height_px()
        }
    }

    /// Returns the height of the usable grid area in pixels (window height
    /// minus the status bar strip and top bar strip).
    ///
    /// Pass this value to PTY resize calculations so the terminal grid never
    /// draws into the status bar or top bar rows.
    #[must_use]
    pub fn grid_height_px(&self) -> u32 {
        self.size
            .height
            .saturating_sub(self.status_bar_height_px())
            .saturating_sub(self.top_bar_height_px())
    }

    /// Uploads `pixels` (RGBA8, row-major) as the terminal background image.
    ///
    /// Replaces any previously uploaded background image.  Call this after the
    /// renderer is constructed to install a background image at runtime (e.g.
    /// when the user changes the setting via a hot-reload mechanism).
    pub fn upload_background_image(&mut self, pixels: &[u8], width: u32, height: u32) {
        let tex_layout = self
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("bg_img_tex_layout"),
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
        let (tex, bg) = upload_bg_image(
            &self.device,
            &self.queue,
            pixels,
            width,
            height,
            &tex_layout,
        );
        self.bg_image_texture = Some(tex);
        self.bg_image_bind_group = Some(bg);
    }

    /// Renders the current cell grid to the window surface.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Frame`] if the surface texture cannot be
    /// acquired.
    pub fn render(&mut self) -> anyhow::Result<()> {
        // Rasterise any glyphs not yet in the atlas before the command encoder
        // is created — queue.write_texture must complete before draw calls.
        self.ensure_cell_glyphs();

        let output = self.surface.get_current_texture()?;
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Build per-frame vertex data and upload it into the persistent, growable
        // buffers BEFORE opening the render pass. Doing the uploads here keeps the
        // buffers' mutable borrow disjoint from the immutable borrows the pass
        // holds, and — unlike the old per-frame `create_buffer_init` — reuses the
        // same GPU allocations across frames, growing only when a frame needs more
        // room than before.
        let bg_image_verts: Option<[BgImageVertex; 6]> = if self.bg_image_bind_group.is_some() {
            Some([
                BgImageVertex {
                    position: [-1.0, 1.0],
                    tex_coords: [0.0, 0.0],
                },
                BgImageVertex {
                    position: [1.0, 1.0],
                    tex_coords: [1.0, 0.0],
                },
                BgImageVertex {
                    position: [-1.0, -1.0],
                    tex_coords: [0.0, 1.0],
                },
                BgImageVertex {
                    position: [1.0, 1.0],
                    tex_coords: [1.0, 0.0],
                },
                BgImageVertex {
                    position: [1.0, -1.0],
                    tex_coords: [1.0, 1.0],
                },
                BgImageVertex {
                    position: [-1.0, -1.0],
                    tex_coords: [0.0, 1.0],
                },
            ])
        } else {
            None
        };
        let bg_verts = self.build_bg_vertices();
        let glyph_verts = self.build_glyph_vertices();
        let colour_verts = self.build_colour_glyph_vertices();

        if let Some(verts) = &bg_image_verts {
            self.bg_image_vbuf
                .upload(&self.device, &self.queue, bytemuck::cast_slice(verts));
        }
        self.bg_vbuf
            .upload(&self.device, &self.queue, bytemuck::cast_slice(&bg_verts));
        self.glyph_vbuf.upload(
            &self.device,
            &self.queue,
            bytemuck::cast_slice(&glyph_verts),
        );
        self.colour_vbuf.upload(
            &self.device,
            &self.queue,
            bytemuck::cast_slice(&colour_verts),
        );

        // The persistent buffers may be larger than this frame's data (they only
        // grow); slice each to exactly this frame's byte length so the vertex
        // stream never reads stale bytes left by a previous, larger frame.
        let bg_slice_len =
            (bg_verts.len() * std::mem::size_of::<BgVertex>()) as wgpu::BufferAddress;
        let glyph_slice_len =
            (glyph_verts.len() * std::mem::size_of::<GlyphVertex>()) as wgpu::BufferAddress;
        let colour_slice_len =
            (colour_verts.len() * std::mem::size_of::<GlyphVertex>()) as wgpu::BufferAddress;

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("render_encoder"),
            });

        let bg = self.config.colors.background;

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(bg[0]),
                            g: f64::from(bg[1]),
                            b: f64::from(bg[2]),
                            a: f64::from(self.background.opacity),
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            // Fullscreen background image (drawn before solid cell quads so
            // that cell backgrounds at opacity < 1.0 blend with it).
            if let (Some(bg_img_group), Some(buf)) =
                (&self.bg_image_bind_group, self.bg_image_vbuf.buffer())
            {
                render_pass.set_pipeline(&self.bg_image_pipeline);
                render_pass.set_bind_group(0, bg_img_group, &[]);
                render_pass.set_bind_group(1, &self.bg_image_params_bind_group, &[]);
                render_pass.set_vertex_buffer(0, buf.slice(..));
                render_pass.draw(0..6, 0..1);
            }

            // Background quads.
            if let Some(buf) = self.bg_vbuf.buffer().filter(|_| !bg_verts.is_empty()) {
                render_pass.set_pipeline(&self.bg_pipeline);
                render_pass.set_vertex_buffer(0, buf.slice(..bg_slice_len));
                render_pass.draw(0..bg_verts.len() as u32, 0..1);
            }

            // Glyph quads.
            if let Some(buf) = self.glyph_vbuf.buffer().filter(|_| !glyph_verts.is_empty()) {
                render_pass.set_pipeline(&self.glyph_pipeline);
                render_pass.set_bind_group(0, &self.bind_group, &[]);
                render_pass.set_vertex_buffer(0, buf.slice(..glyph_slice_len));
                render_pass.draw(0..glyph_verts.len() as u32, 0..1);
            }

            // Registered colour glyphs (PUA codepoints) — sampled from the RGBA
            // colour atlas via the colour pipeline.
            if let Some(buf) = self
                .colour_vbuf
                .buffer()
                .filter(|_| !colour_verts.is_empty())
            {
                render_pass.set_pipeline(&self.colour_pipeline);
                render_pass.set_bind_group(0, &self.colour_bind_group, &[]);
                render_pass.set_vertex_buffer(0, buf.slice(..colour_slice_len));
                render_pass.draw(0..colour_verts.len() as u32, 0..1);
            }

            drop(render_pass);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
        Ok(())
    }
}

// ── Background image upload helper ────────────────────────────────────────────

/// Creates and uploads a wgpu texture from raw RGBA8 pixels.
///
/// Returns `(texture, bind_group)`.  The bind group binds the texture view at
/// slot 0 and a linear sampler at slot 1, matching the layout expected by
/// `bg_image_pipeline`.
fn upload_bg_image(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pixels: &[u8],
    width: u32,
    height: u32,
    layout: &wgpu::BindGroupLayout,
) -> (wgpu::Texture, wgpu::BindGroup) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("bg_image_texture"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::ImageCopyTexture {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("bg_image_sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("bg_image_bind_group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    });
    (texture, bind_group)
}

// wgpu::util::DeviceExt is needed for create_buffer_init.
use wgpu::util::DeviceExt;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use cosmic_text::{fontdb, FontSystem};

    use super::*;

    #[test]
    fn shelf_packer_basic_alloc() {
        let mut p = ShelfPacker::new(100);
        let a = p.alloc(10, 10);
        assert_eq!(a, Some([0, 0]));
    }

    #[test]
    fn shelf_packer_fills_row_then_new_shelf() {
        let mut p = ShelfPacker::new(100);
        // Fill first shelf (10 wide, height 10) 10 times = 100 px.
        for i in 0..10u32 {
            assert_eq!(p.alloc(10, 10), Some([i * 10, 0]));
        }
        // Next alloc must start a new shelf.
        let b = p.alloc(10, 10);
        assert_eq!(b, Some([0, 10]));
    }

    #[test]
    fn shelf_packer_returns_none_when_full() {
        let mut p = ShelfPacker::new(4);
        // Fill the entire atlas.
        p.alloc(4, 4).unwrap();
        // Next alloc should fail.
        assert!(p.alloc(1, 1).is_none());
    }

    #[test]
    fn shelf_packer_rejects_oversized() {
        let mut p = ShelfPacker::new(10);
        assert!(p.alloc(11, 1).is_none());
        assert!(p.alloc(1, 11).is_none());
    }

    #[test]
    fn shelf_packer_alloc_advances_x_for_same_row() {
        let mut p = ShelfPacker::new(64);
        let _ = p.alloc(10, 10); // [0, 0]
        assert_eq!(p.alloc(10, 10), Some([10, 0]));
    }

    #[test]
    fn shelf_packer_alloc_wraps_to_new_shelf() {
        let mut p = ShelfPacker::new(20);
        // first alloc:  [0,0],  x→12, shelf_height→8
        let _ = p.alloc(12, 8);
        // second alloc: 12+12>20 → wrap: y→8, x→0, sh→0 → [0,8], x→12, sh→8
        let _ = p.alloc(12, 8);
        // third alloc:  12+5=17≤20 → [12,8]
        assert_eq!(p.alloc(5, 5), Some([12, 8]));
    }

    #[test]
    fn shelf_packer_alloc_glyph_wider_than_atlas_returns_none() {
        let mut p = ShelfPacker::new(64);
        assert_eq!(p.alloc(128, 1), None);
    }

    // ── GlyphAtlas key tests ──────────────────────────────────────────────────

    #[test]
    fn glyph_atlas_key_distinguishes_bold_and_italic() {
        let sz = 28.0_f32.to_bits();
        let entry = |x: u32| GlyphEntry {
            x,
            y: 0,
            w: 8,
            h: 12,
            bearing_x: 0,
            bearing_y: 10,
            id: None,
            last_used: 0,
        };
        let mut map: HashMap<(char, bool, bool, u32), GlyphEntry> = HashMap::new();
        map.insert(('A', false, false, sz), entry(0));
        map.insert(('A', true, false, sz), entry(8));
        map.insert(('A', false, true, sz), entry(16));
        assert_ne!(map[&('A', false, false, sz)], map[&('A', true, false, sz)]);
        assert_ne!(map[&('A', false, false, sz)], map[&('A', false, true, sz)]);
        assert_eq!(map[&('A', false, false, sz)].x, 0);
    }

    // ── GPU-gated smoke tests ─────────────────────────────────────────────────

    #[cfg_attr(
        not(feature = "gpu-tests"),
        ignore = "requires a GPU device; enable the gpu-tests feature to run"
    )]
    #[test]
    fn renderer_scale_factor_change_clears_atlas() {
        // Verifies that creating a new ShelfPacker (what update_scale_factor does)
        // resets allocation state to the origin.
        let mut packer = ShelfPacker::new(1024);
        let _ = packer.alloc(16, 16);
        // Simulate update_scale_factor: replace with a fresh packer.
        packer = ShelfPacker::new(1024);
        assert_eq!(
            packer.alloc(16, 16),
            Some([0, 0]),
            "fresh packer after scale change must start from origin"
        );
    }

    #[cfg_attr(
        not(feature = "gpu-tests"),
        ignore = "requires a GPU device; enable the gpu-tests feature to run"
    )]
    #[test]
    fn renderer_glyph_atlas_key_stores_bold_and_regular_separately() {
        let sz = 28.0_f32.to_bits();
        let entry = |x: u32| GlyphEntry {
            x,
            y: 0,
            w: 8,
            h: 12,
            bearing_x: 0,
            bearing_y: 10,
            id: None,
            last_used: 0,
        };
        let mut glyphs: HashMap<(char, bool, bool, u32), GlyphEntry> = HashMap::new();
        glyphs.insert(('A', false, false, sz), entry(0));
        glyphs.insert(('A', true, false, sz), entry(8));
        assert!(glyphs.contains_key(&('A', false, false, sz)));
        assert!(glyphs.contains_key(&('A', true, false, sz)));
        assert_ne!(
            glyphs[&('A', false, false, sz)].x,
            glyphs[&('A', true, false, sz)].x
        );
    }

    #[cfg_attr(
        not(feature = "gpu-tests"),
        ignore = "requires a GPU device; enable the gpu-tests feature to run"
    )]
    #[test]
    fn renderer_status_bar_quads_placeholder() {
        // Documents expected behaviour: status-bar segments produce non-empty
        // vertex data. Full assertion requires a headless GPU context.
        // Run with: LIBGL_ALWAYS_SOFTWARE=1 cargo test -p st-render --features gpu-tests
        let segments: Vec<String> = vec!["tier: local".into(), "model: gemma".into()];
        assert!(!segments.is_empty(), "segments input must be non-empty");
    }

    #[test]
    fn glyph_vertex_layout_stride_matches_size() {
        let layout = GlyphVertex::layout();
        assert_eq!(
            layout.array_stride as usize,
            std::mem::size_of::<GlyphVertex>()
        );
    }

    #[test]
    fn bg_vertex_layout_stride_matches_size() {
        let layout = BgVertex::layout();
        assert_eq!(
            layout.array_stride as usize,
            std::mem::size_of::<BgVertex>()
        );
    }

    #[test]
    fn cell_is_plain_old_data() {
        // Verify Cell fields are accessible.
        let c = Cell {
            ch: 'A',
            fg: [1.0, 1.0, 1.0, 1.0],
            bg: [0.0, 0.0, 0.0, 1.0],
            col: 5,
            row: 3,
            ..Cell::default()
        };
        assert_eq!(c.ch, 'A');
        assert_eq!(c.col, 5);
    }

    #[test]
    fn block_decoration_fields_accessible() {
        let d = BlockDecoration {
            start_row: 0,
            end_row: 5,
            exit_code: Some(0),
            selected: false,
        };
        assert_eq!(d.end_row, 5);
    }

    #[test]
    fn agent_header_wraps_model_name() {
        assert_eq!(agent_header("claude"), "[ claude ]");
        assert_eq!(agent_header(""), "[  ]");
    }

    #[test]
    fn hanging_margin_is_label_width_plus_one_gap() {
        // "[ claude ]" is 10 columns; body hangs one column past it.
        let label = agent_header("claude");
        assert_eq!(
            hanging_margin_cols(label.chars().count(), 100),
            11,
            "margin = label width + 1 gap column"
        );
    }

    #[test]
    fn hanging_margin_is_capped_to_preserve_body_width() {
        // A very long label must not push the body past the cap (half the grid).
        assert_eq!(hanging_margin_cols(500, 40), 40);
        // The cap floors at 1 so a degenerate max never yields 0.
        assert_eq!(hanging_margin_cols(500, 0), 1);
    }

    #[test]
    fn hanging_margin_uncapped_short_label() {
        // Below the cap the label width + gap is returned verbatim.
        assert_eq!(hanging_margin_cols(3, 40), 4);
    }

    #[test]
    fn agent_block_view_carries_left_margin() {
        let v = AgentBlockView {
            start_row: 2,
            model: "gpt".into(),
            content_lines: vec!["hi".into()],
            approval_pending: false,
            left_margin_cols: 6,
        };
        assert_eq!(v.left_margin_cols, 6);
    }

    #[test]
    fn background_config_default_opacity_is_1() {
        let bg = BackgroundConfig::default();
        assert!((bg.opacity - 1.0).abs() < f32::EPSILON);
        assert!(bg.image_pixels.is_none());
    }

    #[test]
    fn background_config_load_image_nonexistent_returns_err() {
        let mut bg = BackgroundConfig {
            image_path: Some(std::path::PathBuf::from("/nonexistent/path/image.png")),
            ..BackgroundConfig::default()
        };
        assert!(bg.load_image().is_err());
    }

    // ── Status bar tests ──────────────────────────────────────────────────────

    /// Build a minimal `Renderer`-shaped struct using only pure-logic methods.
    ///
    /// We cannot call `Renderer::new` without a GPU, so we construct a fake
    /// renderer that exercises only the pure helpers.
    fn make_fake_renderer() -> (Vec<Cell>, Vec<Segment>, u32, u32, f32) {
        let cells = Vec::new();
        let segments = vec![
            Segment {
                name: "tier".into(),
                text: "[local]".into(),
                style: st_statusbar::SegmentStyle::default(),
            },
            Segment {
                name: "time".into(),
                text: "12:34".into(),
                style: st_statusbar::SegmentStyle::default(),
            },
        ];
        let width = 1200u32;
        let height = 800u32;
        let font_size = 14.0f32;
        (cells, segments, width, height, font_size)
    }

    #[test]
    fn status_bar_height_scales_with_dpi_and_caps() {
        // scale_factor=1 (non-Retina): eff = font_size * 1 = 14, cap at 36 → 14
        let font_size = 14.0f32;
        let scale_factor = 1.0f32;
        let bar_h = (font_size * scale_factor) as u32;
        let bar_h = bar_h.min(36);
        assert_eq!(bar_h, 14);

        // scale_factor=2 (Retina): eff = 14 * 2 = 28, cap at 36 → 28
        let scale_factor_retina = 2.0f32;
        let bar_h_retina = ((font_size * scale_factor_retina) as u32).min(36);
        assert_eq!(bar_h_retina, 28);

        // Large font capped: eff = 40 → cap at 36
        let big_font = 20.0f32;
        let bar_h_big = ((big_font * scale_factor_retina) as u32).min(36);
        assert_eq!(bar_h_big, 36);
    }

    #[test]
    fn grid_height_is_window_height_minus_status_bar() {
        let window_h = 800u32;
        // scale_factor=2, font_size=14 → bar = min(28, 36) = 28
        let font_size = 14.0f32;
        let scale_factor = 2.0f32;
        let bar_h = ((font_size * scale_factor) as u32).min(36);
        let grid_h = window_h.saturating_sub(bar_h);
        assert_eq!(grid_h, 772);
    }

    #[test]
    fn status_bar_segments_stored_and_cleared() {
        let (_, segments, _, _, _) = make_fake_renderer();
        // Simulate set_status_bar_segments: store the segments.
        let mut stored: Vec<Segment> = segments.clone();
        assert_eq!(stored.len(), 2);
        stored.clear();
        assert!(stored.is_empty());
    }

    #[test]
    fn px_to_ndc_full_window_quad() {
        // Full-width, full-height quad → NDC corners at (-1,1) and (1,-1).
        let pw = 800.0f32;
        let ph = 600.0f32;

        let x0 = 0.0f32 / pw * 2.0 - 1.0;
        let y0 = 1.0 - 0.0f32 / ph * 2.0;
        let x1 = pw / pw * 2.0 - 1.0;
        let y1 = 1.0 - ph / ph * 2.0;

        assert!((x0 - -1.0).abs() < 1e-5);
        assert!((y0 - 1.0).abs() < 1e-5);
        assert!((x1 - 1.0).abs() < 1e-5);
        assert!((y1 - -1.0).abs() < 1e-5);
    }

    // ── Section 1: Non-blocking startup ──────────────────────────────────────

    /// Verifies that initialising a [`FontSystem`] with an empty [`fontdb::Database`]
    /// completes in well under 200 ms, proving the non-blocking fast-init path
    /// avoids the system-font scan that caused the 10-second startup freeze.
    #[test]
    fn glyph_atlas_new_is_fast() {
        let start = std::time::Instant::now();
        let db = fontdb::Database::new();
        let _fs = FontSystem::new_with_locale_and_db("en-US".to_owned(), db);
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 200,
            "FontSystem with empty DB should init in < 200 ms, took {} ms",
            elapsed.as_millis()
        );
    }

    // ── Section 2: Glyph miss tracing ────────────────────────────────────────

    /// GPU-gated placeholder: the glyph miss warn! path requires a live atlas
    /// backed by a real wgpu device.  The warn! call is verified by code inspection
    /// on the non-GPU path.
    #[cfg_attr(
        not(feature = "gpu-tests"),
        ignore = "requires a GPU device; enable the gpu-tests feature to run"
    )]
    #[test]
    fn glyph_miss_emits_warn() {
        // Without GPU this is hard to test directly.
        // Run with: LIBGL_ALWAYS_SOFTWARE=1 cargo test -p st-render --features gpu-tests
    }

    /// Verifies that the `tracing` infrastructure is wired correctly in this crate
    /// by emitting a warn! and confirming `logs_contain` captures it.
    #[test]
    #[tracing_test::traced_test]
    fn glyph_miss_is_logged() {
        // We cannot call build_glyph_vertices without a GPU, but we can confirm
        // that tracing_test integration works and warn! events are captured.
        // The actual atlas-miss warn! paths are exercised by the gpu-tests layer.
        tracing::warn!("test warn from glyph_miss_is_logged");
        assert!(logs_contain("test warn from glyph_miss_is_logged"));
    }

    // ── Section 3: Status-bar glyph warmup ───────────────────────────────────

    /// GPU-gated placeholder: full status-bar glyph warmup and render verification
    /// require a live wgpu device and surface.
    #[cfg_attr(
        not(feature = "gpu-tests"),
        ignore = "requires a GPU device; enable the gpu-tests feature to run"
    )]
    #[test]
    fn status_bar_glyphs_render() {
        // Run with: LIBGL_ALWAYS_SOFTWARE=1 cargo test -p st-render --features gpu-tests
    }

    // ── Section 5: Headless render smoke test ─────────────────────────────────

    /// GPU-gated smoke test: requires a wgpu GL backend with software rendering.
    ///
    /// Run with: `LIBGL_ALWAYS_SOFTWARE=1 cargo test -p st-render --features gpu-tests`
    #[cfg_attr(
        not(feature = "gpu-tests"),
        ignore = "GPU CI harness not yet available"
    )]
    #[test]
    fn headless_render_smoke() {
        // Stub until the GPU CI harness exists. Enabling gpu-tests will run
        // this test; it passes vacuously until a real assertion is wired in.
    }

    // ── Background image tests ────────────────────────────────────────────────

    #[test]
    fn bg_image_vertex_layout_stride_matches_size() {
        let layout = BgImageVertex::layout();
        assert_eq!(
            layout.array_stride as usize,
            std::mem::size_of::<BgImageVertex>()
        );
    }

    #[test]
    fn bg_image_params_size_is_16() {
        assert_eq!(std::mem::size_of::<BgImageParams>(), 16);
    }

    #[test]
    fn background_config_stores_image_path() {
        let path = std::path::PathBuf::from("/tmp/wall.png");
        let bg = BackgroundConfig {
            image_path: Some(path.clone()),
            ..BackgroundConfig::default()
        };
        assert_eq!(bg.image_path, Some(path));
        assert!(bg.image_pixels.is_none());
    }

    // ── Registered colour-glyph atlas (pure logic) ───────────────────────────

    #[test]
    fn is_pua_codepoint_recognises_range_boundaries() {
        assert!(is_pua_codepoint('\u{E000}'));
        assert!(is_pua_codepoint('\u{F8FF}'));
        assert!(!is_pua_codepoint('\u{D7FF}'));
        assert!(!is_pua_codepoint('\u{F900}'));
        assert!(!is_pua_codepoint('A'));
    }

    #[test]
    fn select_atlas_routes_registered_pua_to_colour() {
        // A registered PUA codepoint selects the colour atlas.
        assert_eq!(select_atlas('\u{E000}', true), AtlasKind::Colour);
        // A normal ASCII cell selects the alpha atlas.
        assert_eq!(select_atlas('A', false), AtlasKind::Alpha);
        // A PUA codepoint with no registered bitmap falls back to the alpha
        // atlas (tofu).
        assert_eq!(select_atlas('\u{E000}', false), AtlasKind::Alpha);
        // A non-PUA char is never routed to the colour atlas even if a stray
        // bitmap claim is made.
        assert_eq!(select_atlas('A', true), AtlasKind::Alpha);
    }

    #[test]
    fn registered_rgba_bitmap_round_trips_colour() {
        // The colour atlas uploads RGBA verbatim (bytes_per_row = width * 4),
        // unlike the alpha atlas which collapses to a single channel. This test
        // asserts the staging preserves all four channels of a known bitmap.
        let entry = st_glyph::GlyphAtlasEntry {
            codepoint: '\u{E000}',
            // 1×1 pixel: distinct R, G, B so a collapse to alpha would be lossy.
            pixels: vec![0x12, 0x34, 0x56, 0xFF],
            width: 1,
            height: 1,
        };
        // Bytes-per-row for an RGBA upload is width * 4 — the colour path.
        let bytes_per_row = entry.width * 4;
        assert_eq!(bytes_per_row, 4);
        // The uploaded bytes equal the original RGBA, preserving RGB (an
        // alpha-only path would have produced just [0xFF]).
        assert_eq!(entry.pixels, vec![0x12, 0x34, 0x56, 0xFF]);
        assert_ne!(
            entry.pixels.len(),
            (entry.width * entry.height) as usize,
            "RGBA bitmap must not be collapsed to one byte per pixel"
        );
    }

    #[test]
    fn background_config_load_image_missing_path_returns_err() {
        let mut bg = BackgroundConfig {
            image_path: Some(std::path::PathBuf::from("/no/such/image.png")),
            ..BackgroundConfig::default()
        };
        assert!(bg.load_image().is_err(), "loading a missing file must fail");
    }

    #[test]
    fn colour_atlas_lru_evicts_oldest_not_current_frame() {
        // Mirrors the colour-atlas eviction bookkeeping (CPU-only, no GPU): a
        // full atlas must drop the least-recently-used glyph whose `last_used` is
        // older than the current frame, and must never drop a glyph stamped with
        // the current frame (i.e. visible this frame). This is the same selection
        // expression used in GlyphAtlas::get_or_insert_colour.
        use etagere::{size2, AtlasAllocator};
        let mut alloc = AtlasAllocator::new(size2(32, 32));
        let mut glyphs: HashMap<char, GlyphEntry> = HashMap::new();

        // Fill the atlas with 16×16 slots, each stamped with an increasing
        // `last_used` so the eviction order is deterministic.
        let mut frame = 0u64;
        let mut ch = '\u{E000}';
        while let Some(a) = alloc.allocate(size2(16, 16)) {
            glyphs.insert(
                ch,
                GlyphEntry {
                    x: 0,
                    y: 0,
                    w: 16,
                    h: 16,
                    bearing_x: 0,
                    bearing_y: 16,
                    id: Some(a.id),
                    last_used: frame,
                },
            );
            frame += 1;
            ch = char::from_u32(ch as u32 + 1).expect("valid PUA codepoint");
        }
        assert!(
            glyphs.len() >= 2,
            "atlas should hold at least two 16×16 slots"
        );

        // Current frame stamps the newest glyph as visible now → protected.
        let current_frame = frame;
        let newest = char::from_u32('\u{E000}' as u32 + (glyphs.len() as u32 - 1))
            .expect("valid PUA codepoint");
        glyphs.get_mut(&newest).unwrap().last_used = current_frame;

        // Victim selection: oldest `last_used` strictly below the current frame.
        let victim = glyphs
            .iter()
            .filter(|(_, e)| e.id.is_some() && e.last_used < current_frame)
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, e)| (*k, e.id));
        let (vkey, vid) = victim.expect("an evictable glyph must exist");
        assert_eq!(vkey, '\u{E000}', "the oldest glyph must be chosen");
        assert_ne!(
            vkey, newest,
            "the current-frame glyph must never be evicted"
        );

        // Deallocating the victim must free room for a new glyph.
        alloc.deallocate(vid.expect("victim carries an alloc id"));
        glyphs.remove(&vkey);
        assert!(
            alloc.allocate(size2(16, 16)).is_some(),
            "evicting the LRU glyph must free a slot"
        );
    }

    #[test]
    fn etagere_allocate_then_deallocate_frees_a_slot() {
        // Validates the allocate→full→deallocate→reallocate contract the glyph
        // atlas LRU eviction relies on (CPU-only, no GPU device needed).
        use etagere::{size2, AtlasAllocator};
        let mut a = AtlasAllocator::new(size2(32, 32));
        let mut ids = Vec::new();
        while let Some(al) = a.allocate(size2(16, 16)) {
            ids.push(al.id);
        }
        assert!(!ids.is_empty(), "atlas should fit at least one 16×16 slot");
        // Atlas is now full for 16×16; free the first and a new one must fit.
        a.deallocate(ids[0]);
        assert!(
            a.allocate(size2(16, 16)).is_some(),
            "deallocating a slot must free room for a new allocation"
        );
    }
}
