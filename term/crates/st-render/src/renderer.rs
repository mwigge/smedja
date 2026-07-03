//! The primary [`Renderer`]: owns the wgpu surface, pipelines, and glyph atlas.

use st_statusbar::Segment;
use wgpu::util::DeviceExt;

use crate::atlas::GlyphAtlas;
use crate::background::BackgroundConfig;
use crate::cell::{AgentBlockView, BlockDecoration, Cell};
use crate::error::RenderError;
use crate::shader::{BG_IMAGE_SHADER_SRC, BG_SHADER_SRC, COLOUR_SHADER_SRC, SHADER_SRC};
use crate::vertex::{BgImageParams, BgImageVertex, BgVertex, GlyphVertex};

/// The primary renderer: owns the wgpu surface, pipelines, and glyph atlas.
pub struct Renderer {
    pub(crate) surface: wgpu::Surface<'static>,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) surface_config: wgpu::SurfaceConfiguration,
    pub(crate) glyph_pipeline: wgpu::RenderPipeline,
    pub(crate) bg_pipeline: wgpu::RenderPipeline,
    /// Pipeline for blitting the optional background image.
    pub(crate) bg_image_pipeline: wgpu::RenderPipeline,
    /// Uploaded background image texture (None if no image is configured).
    pub(crate) bg_image_texture: Option<wgpu::Texture>,
    /// Bind group containing the background image texture and sampler.
    pub(crate) bg_image_bind_group: Option<wgpu::BindGroup>,
    /// 16-byte uniform buffer carrying the opacity value for the image pass.
    #[allow(dead_code)] // held for RAII lifetime; Drop releases the GPU allocation
    pub(crate) bg_image_params_buf: wgpu::Buffer,
    /// Bind group for [`Self::bg_image_params_buf`].
    pub(crate) bg_image_params_bind_group: wgpu::BindGroup,
    pub(crate) atlas: GlyphAtlas,
    pub(crate) bind_group: wgpu::BindGroup,
    /// Pipeline for registered RGBA colour glyphs.
    pub(crate) colour_pipeline: wgpu::RenderPipeline,
    /// Bind group binding the colour atlas texture + sampler.
    pub(crate) colour_bind_group: wgpu::BindGroup,
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
    // ponytail: must be last — Instance owns the EGLDisplay/Wayland connection;
    // all GPU resources hold internal back-refs into it and must drop first.
    pub(crate) _instance: wgpu::Instance,
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
}

/// Creates and uploads a wgpu texture from raw RGBA8 pixels.
///
/// Returns `(texture, bind_group)`.  The bind group binds the texture view at
/// slot 0 and a linear sampler at slot 1, matching the layout expected by
/// `bg_image_pipeline`.
pub(crate) fn upload_bg_image(
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
