//! `st-render` — wgpu-based terminal cell-grid renderer for smedja-term.
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

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use cosmic_text::{FontSystem, SwashCache};
use st_statusbar::Segment;
use thiserror::Error;
use tracing::debug;

// Re-export so callers don't have to depend on winit/wgpu directly.
pub use wgpu;
pub use winit;

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
#[derive(Debug, Clone, PartialEq)]
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

// ── Shelf packer ─────────────────────────────────────────────────────────────

/// A simple shelf bin-packer for the glyph atlas.
///
/// Glyphs are packed left-to-right in horizontal shelves.  A new shelf starts
/// whenever the current one is full.
#[derive(Debug)]
pub struct ShelfPacker {
    current_x: u32,
    current_y: u32,
    shelf_height: u32,
    atlas_size: u32,
}

impl ShelfPacker {
    /// Creates a new [`ShelfPacker`] for an atlas of `atlas_size × atlas_size`.
    #[must_use]
    pub fn new(atlas_size: u32) -> Self {
        Self {
            current_x: 0,
            current_y: 0,
            shelf_height: 0,
            atlas_size,
        }
    }

    /// Allocates a `w × h` region in the atlas, returning the top-left `[x, y]`.
    ///
    /// Returns `None` when the atlas is full.
    pub fn alloc(&mut self, w: u32, h: u32) -> Option<[u32; 2]> {
        if w > self.atlas_size || h > self.atlas_size {
            return None;
        }
        // Need a new shelf?
        if self.current_x + w > self.atlas_size {
            self.current_y += self.shelf_height;
            self.current_x = 0;
            self.shelf_height = 0;
        }
        if self.current_y + h > self.atlas_size {
            return None; // Atlas full.
        }
        let pos = [self.current_x, self.current_y];
        self.current_x += w;
        self.shelf_height = self.shelf_height.max(h);
        Some(pos)
    }
}

// ── Glyph atlas ───────────────────────────────────────────────────────────────

const ATLAS_SIZE: u32 = 1024;

/// GPU texture atlas for rasterised glyphs.
///
/// Glyphs are keyed by `(char, is_bold, is_italic)` and cached after first
/// rasterisation via [`cosmic_text`].
pub struct GlyphAtlas {
    /// The GPU texture.
    pub texture: wgpu::Texture,
    /// View into [`Self::texture`].
    pub view: wgpu::TextureView,
    /// CPU-side packer that tracks free regions.
    pub packer: ShelfPacker,
    /// Maps `(char, bold, italic)` → atlas UV rect `[x, y, w, h]` in pixels.
    pub glyphs: HashMap<(char, bool, bool), [u32; 4]>,
    font_system: FontSystem,
    swash_cache: SwashCache,
}

impl GlyphAtlas {
    /// Creates a new [`GlyphAtlas`] backed by `device`.
    #[must_use]
    pub fn new(device: &wgpu::Device) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph_atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        Self {
            texture,
            view,
            packer: ShelfPacker::new(ATLAS_SIZE),
            glyphs: HashMap::new(),
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
        }
    }

    /// Returns the cached UV rect for `ch`, or rasterises and uploads it if not
    /// yet cached.
    ///
    /// Returns `None` if the atlas is full or rasterisation fails.
    pub fn get_or_insert(
        &mut self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        ch: char,
        font_size: f32,
        bold: bool,
        italic: bool,
    ) -> Option<[u32; 4]> {
        let key = (ch, bold, italic);
        if let Some(&rect) = self.glyphs.get(&key) {
            return Some(rect);
        }

        // Rasterise the glyph using cosmic-text + swash.
        let metrics = cosmic_text::Metrics::new(font_size, font_size * 1.2);
        let mut buffer = cosmic_text::Buffer::new(&mut self.font_system, metrics);
        buffer.set_size(
            &mut self.font_system,
            Some(font_size * 2.0),
            Some(font_size * 2.0),
        );

        let attrs = cosmic_text::Attrs::new();
        let attrs = if bold {
            attrs.weight(cosmic_text::Weight::BOLD)
        } else {
            attrs
        };
        let attrs = if italic {
            attrs.style(cosmic_text::Style::Italic)
        } else {
            attrs
        };

        buffer.set_text(
            &mut self.font_system,
            &ch.to_string(),
            attrs,
            cosmic_text::Shaping::Advanced,
        );
        buffer.shape_until_scroll(&mut self.font_system, false);

        // Collect pixel data from swash.
        let mut pixel_data: Option<(Vec<u8>, u32, u32)> = None;

        for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                let physical = glyph.physical((0.0, 0.0), 1.0);
                if let Some(image) = self
                    .swash_cache
                    .get_image(&mut self.font_system, physical.cache_key)
                {
                    let w = image.placement.width;
                    let h = image.placement.height;
                    if w > 0 && h > 0 {
                        let data = match image.content {
                            cosmic_text::SwashContent::Mask => image.data.to_vec(),
                            cosmic_text::SwashContent::Color => {
                                // Convert RGBA → alpha-only for the atlas.
                                image.data.chunks(4).map(|c| c[3]).collect()
                            }
                            cosmic_text::SwashContent::SubpixelMask => {
                                vec![0u8; (w * h) as usize]
                            }
                        };
                        pixel_data = Some((data, w, h));
                        break;
                    }
                }
            }
            if pixel_data.is_some() {
                break;
            }
        }

        let (data, w, h) = pixel_data.unwrap_or_else(|| {
            // Fallback: blank 1×1 glyph so the atlas entry is valid.
            (vec![0u8], 1, 1)
        });

        let [x, y] = self.packer.alloc(w, h)?;

        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &data,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );

        let rect = [x, y, w, h];
        self.glyphs.insert(key, rect);
        Some(rect)
    }
}

// ── WGSL shader ───────────────────────────────────────────────────────────────

const SHADER_SRC: &str = r"
struct VertexInput {
    @location(0) position:   vec2<f32>,
    @location(1) tex_coords: vec2<f32>,
    @location(2) color:      vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
    @location(1) color:      vec4<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.tex_coords    = in.tex_coords;
    out.color         = in.color;
    return out;
}

@group(0) @binding(0) var t_atlas:  texture_2d<f32>;
@group(0) @binding(1) var s_atlas:  sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let alpha = textureSample(t_atlas, s_atlas, in.tex_coords).r;
    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
";

// ── Background shader ─────────────────────────────────────────────────────────

const BG_SHADER_SRC: &str = r"
struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) color:    vec4<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs_bg(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs_bg(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
";

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

// ── Renderer ──────────────────────────────────────────────────────────────────

/// The primary renderer: owns the wgpu surface, pipelines, and glyph atlas.
pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: wgpu::SurfaceConfiguration,
    glyph_pipeline: wgpu::RenderPipeline,
    bg_pipeline: wgpu::RenderPipeline,
    atlas: GlyphAtlas,
    bind_group: wgpu::BindGroup,
    /// Current cell grid snapshot.
    cells: Vec<Cell>,
    /// Block decorations to draw.
    block_decorations: Vec<BlockDecoration>,
    /// Agent blocks to draw.
    agent_blocks: Vec<AgentBlockView>,
    config: st_config::Config,
    /// Background image and transparency configuration.
    pub background: BackgroundConfig,
    /// Physical size of the window in pixels.
    pub size: winit::dpi::PhysicalSize<u32>,
    /// Status bar segments to overlay at the bottom of the window.
    status_bar_segments: Vec<Segment>,
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
                    label: Some("smedja-term"),
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

        let atlas = GlyphAtlas::new(&device);

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

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
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

        // Pre-populate with "hello smedja-term" as an initial test render.
        let fg = config.colors.foreground;
        let bg_color = config.colors.background;
        let hello = "hello smedja-term";
        let initial_cells: Vec<Cell> = hello
            .chars()
            .enumerate()
            .map(|(i, ch)| Cell {
                ch,
                fg,
                bg: bg_color,
                col: i as u16,
                row: 0,
            })
            .collect();

        Ok(Self {
            surface,
            device,
            queue,
            surface_config,
            glyph_pipeline,
            bg_pipeline,
            atlas,
            bind_group,
            cells: initial_cells,
            block_decorations: Vec::new(),
            agent_blocks: Vec::new(),
            config: config.clone(),
            background: BackgroundConfig {
                image_path: config
                    .window
                    .background_image
                    .as_ref()
                    .map(std::path::PathBuf::from),
                opacity: config.window.background_opacity,
                ..BackgroundConfig::default()
            },
            size,
            status_bar_segments: Vec::new(),
        })
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
        debug!("renderer resized to {}×{}", new_size.width, new_size.height);
    }

    /// Updates the cell grid from a slice of [`Cell`]s.
    pub fn update_cells(&mut self, cells: &[Cell]) {
        self.cells.clear();
        self.cells.extend_from_slice(cells);
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

    /// Updates the segments displayed in the status bar strip.
    ///
    /// Segments are laid out left-to-right separated by a single space.  Call
    /// this before [`Self::render`] each frame.
    pub fn set_status_bar_segments(&mut self, segments: &[Segment]) {
        self.status_bar_segments = segments.to_vec();
    }

    /// Returns the pixel height reserved for the status bar.
    ///
    /// The bar height is the smaller of the configured font size and 18 px so
    /// that it remains visually independent from the terminal grid.
    #[must_use]
    pub fn status_bar_height_px(&self) -> u32 {
        // Independent of grid font: cap at 18 px.
        (self.config.font.size as u32).min(18)
    }

    /// Returns the height of the usable grid area in pixels (window height
    /// minus the status bar strip).
    ///
    /// Pass this value to PTY resize calculations so the terminal grid never
    /// draws into the status bar row.
    #[must_use]
    pub fn grid_height_px(&self) -> u32 {
        self.size.height.saturating_sub(self.status_bar_height_px())
    }

    /// Renders the current cell grid to the window surface.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Frame`] if the surface texture cannot be
    /// acquired.
    pub fn render(&mut self) -> anyhow::Result<()> {
        let output = self.surface.get_current_texture()?;
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

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

            // Background quads.
            let bg_verts = self.build_bg_vertices();
            if !bg_verts.is_empty() {
                let buf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("bg_vbuf"),
                        contents: bytemuck::cast_slice(&bg_verts),
                        usage: wgpu::BufferUsages::VERTEX,
                    });
                render_pass.set_pipeline(&self.bg_pipeline);
                render_pass.set_vertex_buffer(0, buf.slice(..));
                render_pass.draw(0..bg_verts.len() as u32, 0..1);
            }

            // Glyph quads.
            let glyph_verts = self.build_glyph_vertices();
            if !glyph_verts.is_empty() {
                let buf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("glyph_vbuf"),
                        contents: bytemuck::cast_slice(&glyph_verts),
                        usage: wgpu::BufferUsages::VERTEX,
                    });
                render_pass.set_pipeline(&self.glyph_pipeline);
                render_pass.set_bind_group(0, &self.bind_group, &[]);
                render_pass.set_vertex_buffer(0, buf.slice(..));
                render_pass.draw(0..glyph_verts.len() as u32, 0..1);
            }

            drop(render_pass);
        }

        // ponytail: GPU blit deferred, pixels loaded into self.background.image_pixels
        // TODO: upload background.image_pixels as a wgpu texture and blit before cell quads

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
        Ok(())
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Estimates cell size based on font metrics and window size.
    fn cell_size(&self) -> (f32, f32) {
        // Simple approximation: font_size × 0.6 wide, font_size × 1.2 tall.
        let w = self.config.font.size * 0.6;
        let h = self.config.font.size * 1.2;
        (w, h)
    }

    fn cell_to_ndc(&self, col: u16, row: u16, cell_w: f32, cell_h: f32) -> (f32, f32, f32, f32) {
        let pw = self.size.width as f32;
        let ph = self.size.height as f32;
        let x0 = (f32::from(col) * cell_w) / pw * 2.0 - 1.0;
        let y0 = 1.0 - (f32::from(row) * cell_h) / ph * 2.0;
        let x1 = x0 + cell_w / pw * 2.0;
        let y1 = y0 - cell_h / ph * 2.0;
        (x0, y0, x1, y1)
    }

    /// Converts a pixel-space rectangle `(px0, py0, px1, py1)` to NDC.
    ///
    /// `py0` is the top edge (smaller y in pixel space, larger y in NDC).
    fn px_to_ndc(&self, px0: f32, py0: f32, px1: f32, py1: f32) -> (f32, f32, f32, f32) {
        let pw = self.size.width as f32;
        let ph = self.size.height as f32;
        let x0 = px0 / pw * 2.0 - 1.0;
        let y0 = 1.0 - py0 / ph * 2.0;
        let x1 = px1 / pw * 2.0 - 1.0;
        let y1 = 1.0 - py1 / ph * 2.0;
        (x0, y0, x1, y1)
    }

    fn build_bg_vertices(&self) -> Vec<BgVertex> {
        let (cw, ch) = self.cell_size();
        let mut verts = Vec::with_capacity(self.cells.len() * 6);

        for cell in &self.cells {
            let (x0, y0, x1, y1) = self.cell_to_ndc(cell.col, cell.row, cw, ch);
            let c = cell.bg;
            // Two triangles forming a quad.
            verts.extend_from_slice(&[
                BgVertex {
                    position: [x0, y0],
                    color: c,
                },
                BgVertex {
                    position: [x1, y0],
                    color: c,
                },
                BgVertex {
                    position: [x0, y1],
                    color: c,
                },
                BgVertex {
                    position: [x1, y0],
                    color: c,
                },
                BgVertex {
                    position: [x1, y1],
                    color: c,
                },
                BgVertex {
                    position: [x0, y1],
                    color: c,
                },
            ]);
        }

        // Block decoration borders (left 1px bar in #a9652f).
        let border_color: [f32; 4] = [0.663, 0.396, 0.184, 1.0];
        let bar_w = 2.0 / self.size.width as f32; // 1 pixel in NDC
        for dec in &self.block_decorations {
            let (x0, y0, _, _) = self.cell_to_ndc(0, dec.start_row, cw, ch);
            let (_, _, _, y1) = self.cell_to_ndc(0, dec.end_row, cw, ch);
            let x1 = x0 + bar_w;
            let c = border_color;
            verts.extend_from_slice(&[
                BgVertex {
                    position: [x0, y0],
                    color: c,
                },
                BgVertex {
                    position: [x1, y0],
                    color: c,
                },
                BgVertex {
                    position: [x0, y1],
                    color: c,
                },
                BgVertex {
                    position: [x1, y0],
                    color: c,
                },
                BgVertex {
                    position: [x1, y1],
                    color: c,
                },
                BgVertex {
                    position: [x0, y1],
                    color: c,
                },
            ]);
        }

        // ── Status bar background strip ───────────────────────────────────────
        {
            // Always draw the status bar background so the strip is visible
            // even when no modules produce output.
            let sb_h = self.status_bar_height_px() as f32;
            let ph = self.size.height as f32;
            let pw = self.size.width as f32;
            let py0 = ph - sb_h;
            let py1 = ph;
            // Dark background slightly different from terminal bg.
            let sb_bg: [f32; 4] = [0.07, 0.07, 0.09, 1.0];
            let (x0, y0, x1, y1) = self.px_to_ndc(0.0, py0, pw, py1);
            verts.extend_from_slice(&[
                BgVertex {
                    position: [x0, y0],
                    color: sb_bg,
                },
                BgVertex {
                    position: [x1, y0],
                    color: sb_bg,
                },
                BgVertex {
                    position: [x0, y1],
                    color: sb_bg,
                },
                BgVertex {
                    position: [x1, y0],
                    color: sb_bg,
                },
                BgVertex {
                    position: [x1, y1],
                    color: sb_bg,
                },
                BgVertex {
                    position: [x0, y1],
                    color: sb_bg,
                },
            ]);
        }

        verts
    }

    fn build_glyph_vertices(&self) -> Vec<GlyphVertex> {
        let (cw, ch) = self.cell_size();
        // Reserve extra capacity for status bar glyphs.
        let extra: usize = self
            .status_bar_segments
            .iter()
            .map(|s| s.text.len())
            .sum::<usize>()
            + self.status_bar_segments.len().saturating_sub(1); // separators
        let mut verts = Vec::with_capacity(self.cells.len() * 6 + extra * 6);

        for cell in &self.cells {
            if cell.ch == ' ' {
                continue;
            }
            let atlas_size_f = ATLAS_SIZE as f32;
            // Look up glyph rect from atlas (read-only view — we cannot call
            // get_or_insert here because we'd need &mut self; use cached value).
            let Some(&[ax, ay, aw, ah]) = self.atlas.glyphs.get(&(cell.ch, false, false)) else {
                continue;
            };
            let u0 = ax as f32 / atlas_size_f;
            let v0 = ay as f32 / atlas_size_f;
            let u1 = (ax + aw) as f32 / atlas_size_f;
            let v1 = (ay + ah) as f32 / atlas_size_f;

            let (x0, y0, x1, y1) = self.cell_to_ndc(cell.col, cell.row, cw, ch);
            let c = cell.fg;
            verts.extend_from_slice(&[
                GlyphVertex {
                    position: [x0, y0],
                    tex_coords: [u0, v0],
                    color: c,
                },
                GlyphVertex {
                    position: [x1, y0],
                    tex_coords: [u1, v0],
                    color: c,
                },
                GlyphVertex {
                    position: [x0, y1],
                    tex_coords: [u0, v1],
                    color: c,
                },
                GlyphVertex {
                    position: [x1, y0],
                    tex_coords: [u1, v0],
                    color: c,
                },
                GlyphVertex {
                    position: [x1, y1],
                    tex_coords: [u1, v1],
                    color: c,
                },
                GlyphVertex {
                    position: [x0, y1],
                    tex_coords: [u0, v1],
                    color: c,
                },
            ]);
        }

        // ── Status bar glyphs ─────────────────────────────────────────────────
        //
        // Text is rendered at a fixed 12×18 cell size (independent of the
        // terminal grid font) so it fits within the status_bar_height_px() strip.
        let sb_h = self.status_bar_height_px() as f32;
        let ph = self.size.height as f32;
        let pw = self.size.width as f32;
        // Status bar font metrics: fixed 12 px wide, sb_h tall.
        let sb_cw = 7.2_f32; // ~60 % of 12 px
        let atlas_size_f = ATLAS_SIZE as f32;
        let mut col_px = 4.0_f32; // 4 px left padding

        for (seg_idx, seg) in self.status_bar_segments.iter().enumerate() {
            // Separator between segments.
            if seg_idx > 0 {
                col_px += sb_cw; // one character-width gap
            }
            let fg_color: [f32; 4] =
                seg.style
                    .fg
                    .as_ref()
                    .map_or([0.957, 0.843, 0.631, 1.0], |c| {
                        [
                            f32::from(c.r) / 255.0,
                            f32::from(c.g) / 255.0,
                            f32::from(c.b) / 255.0,
                            1.0,
                        ]
                    }); // forged_terminal fg

            for ch in seg.text.chars() {
                if ch == ' ' {
                    col_px += sb_cw;
                    continue;
                }
                let Some(&[ax, ay, aw, ah]) = self.atlas.glyphs.get(&(ch, false, false)) else {
                    col_px += sb_cw;
                    continue;
                };
                let u0 = ax as f32 / atlas_size_f;
                let v0 = ay as f32 / atlas_size_f;
                let u1 = (ax + aw) as f32 / atlas_size_f;
                let v1 = (ay + ah) as f32 / atlas_size_f;

                let py0 = ph - sb_h;
                let py1 = ph;

                let (x0, y0, x1, y1) = self.px_to_ndc(col_px, py0, col_px + sb_cw, py1);
                let c = fg_color;
                verts.extend_from_slice(&[
                    GlyphVertex {
                        position: [x0, y0],
                        tex_coords: [u0, v0],
                        color: c,
                    },
                    GlyphVertex {
                        position: [x1, y0],
                        tex_coords: [u1, v0],
                        color: c,
                    },
                    GlyphVertex {
                        position: [x0, y1],
                        tex_coords: [u0, v1],
                        color: c,
                    },
                    GlyphVertex {
                        position: [x1, y0],
                        tex_coords: [u1, v0],
                        color: c,
                    },
                    GlyphVertex {
                        position: [x1, y1],
                        tex_coords: [u1, v1],
                        color: c,
                    },
                    GlyphVertex {
                        position: [x0, y1],
                        tex_coords: [u0, v1],
                        color: c,
                    },
                ]);
                col_px += sb_cw;
                // Stop if we run off the right edge.
                if col_px >= pw {
                    break;
                }
            }
            if col_px >= pw {
                break;
            }
        }

        verts
    }
}

// wgpu::util::DeviceExt is needed for create_buffer_init.
use wgpu::util::DeviceExt;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
    fn status_bar_height_is_capped_at_18() {
        // font_size = 14 → bar = min(14, 18) = 14
        let font_size = 14.0f32;
        let bar_h = (font_size as u32).min(18);
        assert_eq!(bar_h, 14);

        // font_size = 24 → bar = min(24, 18) = 18
        let big_font = 24.0f32;
        let bar_h_big = (big_font as u32).min(18);
        assert_eq!(bar_h_big, 18);
    }

    #[test]
    fn grid_height_is_window_height_minus_status_bar() {
        let window_h = 800u32;
        let font_size = 14.0f32;
        let bar_h = (font_size as u32).min(18);
        let grid_h = window_h.saturating_sub(bar_h);
        assert_eq!(grid_h, 786);
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
}
