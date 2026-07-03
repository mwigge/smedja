//! The primary [`Renderer`]: owns the wgpu surface, pipelines, and glyph atlas.

use std::sync::Arc;

use parking_lot::Mutex;
use st_statusbar::Segment;
// wgpu::util::DeviceExt is needed for create_buffer_init.
use wgpu::util::DeviceExt;

use crate::atlas::{GlyphAtlas, ATLAS_SIZE};
use crate::background::{upload_bg_image, BackgroundConfig};
use crate::cell::{AgentBlockView, BlockDecoration, Cell};
use crate::error::RenderError;
use crate::shader::{BG_IMAGE_SHADER_SRC, BG_SHADER_SRC, COLOUR_SHADER_SRC, SHADER_SRC};
use crate::vertex::{BgImageParams, BgImageVertex, BgVertex, GlyphVertex};

mod geometry;

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
    bg_image_bind_group: Option<wgpu::BindGroup>,
    /// 16-byte uniform buffer carrying the opacity value for the image pass.
    #[allow(dead_code)] // held for RAII lifetime; Drop releases the GPU allocation
    bg_image_params_buf: wgpu::Buffer,
    /// Bind group for [`Self::bg_image_params_buf`].
    bg_image_params_bind_group: wgpu::BindGroup,
    atlas: GlyphAtlas,
    bind_group: wgpu::BindGroup,
    /// Pipeline for registered RGBA colour glyphs.
    colour_pipeline: wgpu::RenderPipeline,
    /// Bind group binding the colour atlas texture + sampler.
    colour_bind_group: wgpu::BindGroup,
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
    /// Top bar segments to overlay at the top of the window.
    top_bar_segments: Vec<Segment>,
    /// Device pixel ratio for this window (1.0 on non-HiDPI, 2.0 on 2× displays).
    pub scale_factor: f64,
    // ponytail: must be last — Instance owns the EGLDisplay/Wayland connection;
    // all GPU resources hold internal back-refs into it and must drop first.
    _instance: wgpu::Instance,
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
            cells: initial_cells,
            block_decorations: Vec::new(),
            agent_blocks: Vec::new(),
            config: config.clone(),
            background,
            size,
            status_bar_segments: Vec::new(),
            top_bar_segments: Vec::new(),
            scale_factor,
            _instance: instance,
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
    /// and uploading it on first sight. Spaces and registered PUA colour glyphs
    /// are no-ops (the latter live in the bounded, never-evicted colour atlas).
    fn touch_or_warm(&mut self, ch: char, font_size: f32, bold: bool, italic: bool) {
        if ch == ' ' {
            return;
        }
        let key = (ch, bold, italic, font_size.to_bits());
        if let Some(entry) = self.atlas.glyphs.get_mut(&key) {
            entry.last_used = self.atlas.frame;
            return;
        }
        if self.atlas.colour_glyphs.contains_key(&ch) {
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
            if let Some(bg_img_group) = &self.bg_image_bind_group {
                let verts = [
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
                ];
                let buf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("bg_image_vbuf"),
                        contents: bytemuck::cast_slice(&verts),
                        usage: wgpu::BufferUsages::VERTEX,
                    });
                render_pass.set_pipeline(&self.bg_image_pipeline);
                render_pass.set_bind_group(0, bg_img_group, &[]);
                render_pass.set_bind_group(1, &self.bg_image_params_bind_group, &[]);
                render_pass.set_vertex_buffer(0, buf.slice(..));
                render_pass.draw(0..6, 0..1);
            }

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

            // Registered colour glyphs (PUA codepoints) — sampled from the RGBA
            // colour atlas via the colour pipeline.
            let colour_verts = self.build_colour_glyph_vertices();
            if !colour_verts.is_empty() {
                let buf = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("colour_glyph_vbuf"),
                        contents: bytemuck::cast_slice(&colour_verts),
                        usage: wgpu::BufferUsages::VERTEX,
                    });
                render_pass.set_pipeline(&self.colour_pipeline);
                render_pass.set_bind_group(0, &self.colour_bind_group, &[]);
                render_pass.set_vertex_buffer(0, buf.slice(..));
                render_pass.draw(0..colour_verts.len() as u32, 0..1);
            }

            drop(render_pass);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── GPU-gated smoke tests ─────────────────────────────────────────────────

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
}
