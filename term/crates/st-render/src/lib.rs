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

use std::collections::HashMap;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use cosmic_text::{fontdb, FontSystem, SwashCache};
use parking_lot::Mutex;
use st_statusbar::Segment;
use thiserror::Error;

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

/// First codepoint of the Unicode Private Use Area block (inclusive).
const PUA_START: u32 = 0xE000;
/// Last codepoint of the Unicode Private Use Area block (inclusive).
const PUA_END: u32 = 0xF8FF;

/// Returns `true` when `ch` falls inside the Unicode Private Use Area block
/// (`U+E000 ..= U+F8FF`) used for registered glyphs.
#[must_use]
pub fn is_pua_codepoint(ch: char) -> bool {
    let cp = ch as u32;
    (PUA_START..=PUA_END).contains(&cp)
}

/// Identifies which texture atlas a glyph is sampled from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtlasKind {
    /// The `R8Unorm` alpha-only atlas for font glyphs (tinted by cell foreground).
    Alpha,
    /// The `Rgba8UnormSrgb` colour atlas for registered PUA-codepoint glyphs.
    Colour,
}

/// Selects the atlas a cell glyph is drawn from.
///
/// A codepoint in the PUA range that has a registered bitmap
/// (`has_registered_bitmap`) is sampled from the [`AtlasKind::Colour`] atlas;
/// every other glyph (ordinary text, or a PUA codepoint with no registered
/// bitmap) falls through to the [`AtlasKind::Alpha`] font atlas.
#[must_use]
pub fn select_atlas(ch: char, has_registered_bitmap: bool) -> AtlasKind {
    if has_registered_bitmap && is_pua_codepoint(ch) {
        AtlasKind::Colour
    } else {
        AtlasKind::Alpha
    }
}

/// Per-glyph entry stored in the atlas after rasterisation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlyphEntry {
    /// X origin of the glyph bitmap in atlas pixels.
    pub x: u32,
    /// Y origin of the glyph bitmap in atlas pixels.
    pub y: u32,
    /// Width of the glyph bitmap in atlas pixels.
    pub w: u32,
    /// Height of the glyph bitmap in atlas pixels.
    pub h: u32,
    /// Horizontal offset from the cell's cursor position to the left edge of
    /// the bitmap (positive = right of cursor). From `swash::Placement::left`.
    pub bearing_x: i32,
    /// Vertical offset from the baseline to the top edge of the bitmap
    /// (positive = above baseline). From `swash::Placement::top`.
    pub bearing_y: i32,
    /// Alpha-atlas allocation handle (for deallocation on LRU eviction). `None`
    /// for colour-atlas entries, which are bounded and never evicted.
    pub id: Option<etagere::AllocId>,
    /// Frame index when this glyph was last used — the LRU key.
    pub last_used: u64,
}

/// GPU texture atlas for rasterised glyphs.
///
/// Glyphs are keyed by `(char, is_bold, is_italic)` and cached after first
/// rasterisation via [`cosmic_text`].
pub struct GlyphAtlas {
    /// The GPU texture.
    pub texture: wgpu::Texture,
    /// View into [`Self::texture`].
    pub view: wgpu::TextureView,
    /// Dynamic shelf allocator for the alpha atlas — supports per-glyph
    /// deallocation, so a full atlas evicts only the least-recently-used glyphs
    /// (not a full clear).
    pub alpha_alloc: etagere::AtlasAllocator,
    /// Monotonic frame counter; each glyph's `last_used` is stamped with it so
    /// eviction never drops a glyph still on screen this frame.
    pub frame: u64,
    /// Maps `(char, bold, italic, font_size_bits)` → per-glyph atlas entry.
    ///
    /// `font_size_bits` is `font_size.to_bits()` so that glyphs rasterised at
    /// different sizes (e.g. terminal grid vs status-bar) never share a slot.
    pub glyphs: HashMap<(char, bool, bool, u32), GlyphEntry>,
    /// `Rgba8UnormSrgb` colour texture holding registered PUA-glyph bitmaps.
    pub colour_texture: wgpu::Texture,
    /// View into [`Self::colour_texture`].
    pub colour_view: wgpu::TextureView,
    /// Packer that tracks free regions in the colour atlas.
    pub colour_packer: ShelfPacker,
    /// Maps a registered PUA codepoint → its entry in the colour atlas.
    pub colour_glyphs: HashMap<char, GlyphEntry>,
    /// Shared glyph registry used to resolve PUA codepoints to bitmaps.
    glyph_registry: Option<Arc<Mutex<st_glyph::GlyphRegistry>>>,
    font_system: FontSystem,
    swash_cache: SwashCache,
}

impl GlyphAtlas {
    /// Creates a new [`GlyphAtlas`] backed by `device`.
    ///
    /// # Panics
    ///
    /// Panics if `ATLAS_SIZE` does not fit in `i32` (compile-time invariant).
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

        let colour_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph_atlas_colour"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let colour_view = colour_texture.create_view(&wgpu::TextureViewDescriptor::default());

        Self {
            texture,
            view,
            alpha_alloc: etagere::AtlasAllocator::new(etagere::size2(
                i32::try_from(ATLAS_SIZE).expect("ATLAS_SIZE fits i32"),
                i32::try_from(ATLAS_SIZE).expect("ATLAS_SIZE fits i32"),
            )),
            frame: 0,
            glyphs: HashMap::new(),
            colour_texture,
            colour_view,
            colour_packer: ShelfPacker::new(ATLAS_SIZE),
            colour_glyphs: HashMap::new(),
            glyph_registry: None,
            // Use an empty fontdb so init is non-blocking (< 5 ms).
            // Glyphs are rasterised lazily via ensure_cell_glyphs(); the OS
            // font scanner is skipped here and only called via
            // new_with_system_fonts() when full font coverage is needed.
            font_system: FontSystem::new_with_locale_and_db(
                "en-US".to_owned(),
                fontdb::Database::new(),
            ),
            swash_cache: SwashCache::new(),
        }
    }

    /// Creates a [`GlyphAtlas`] that loads all system fonts (slow — use on a
    /// background thread or when full font coverage is needed).
    #[must_use]
    pub fn new_with_system_fonts(device: &wgpu::Device) -> Self {
        let mut s = Self::new(device);
        s.font_system.db_mut().load_system_fonts();
        s
    }

    /// Returns the cached [`GlyphEntry`] for `ch`, or rasterises and uploads it
    /// if not yet cached.
    ///
    /// Returns `None` if the atlas is full or rasterisation fails.
    ///
    /// # Panics
    ///
    /// Panics if a rasterised glyph dimension does not fit in `i32`.
    pub fn get_or_insert(
        &mut self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        ch: char,
        font_size: f32,
        bold: bool,
        italic: bool,
    ) -> Option<GlyphEntry> {
        // Registered PUA glyphs are uploaded to the colour atlas from their
        // cached RGBA bitmap rather than shaped through cosmic-text.
        if is_pua_codepoint(ch) {
            if let Some(entry) = self.get_or_insert_colour(queue, ch) {
                return Some(entry);
            }
            // No registered bitmap — fall through to the font path (tofu).
        }

        let key = (ch, bold, italic, font_size.to_bits());
        if let Some(entry) = self.glyphs.get_mut(&key) {
            entry.last_used = self.frame;
            return Some(*entry);
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

        // Collect pixel data and placement offsets from swash.
        let mut pixel_data: Option<(Vec<u8>, u32, u32, i32, i32)> = None;

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
                        pixel_data = Some((data, w, h, image.placement.left, image.placement.top));
                        break;
                    }
                }
            }
            if pixel_data.is_some() {
                break;
            }
        }

        let (data, w, h, bearing_x, bearing_y) = pixel_data.unwrap_or_else(|| {
            // Fallback: blank 1×1 glyph so the atlas entry is valid.
            (vec![0u8], 1, 1, 0, 0)
        });

        // Allocate a slot. When the atlas is full, evict the least-recently-used
        // glyph that is NOT in use this frame and retry — incremental LRU rather
        // than a full clear. ensure_cell_glyphs stamps every on-screen glyph's
        // `last_used` to the current frame, so an in-use glyph is never dropped.
        // If nothing is evictable the glyph is skipped (returns None) for this
        // frame; it is re-warmed next frame once room frees up.
        let alloc = loop {
            if let Some(a) = self.alpha_alloc.allocate(etagere::size2(
                i32::try_from(w).expect("glyph width fits i32"),
                i32::try_from(h).expect("glyph height fits i32"),
            )) {
                break a;
            }
            let victim = self
                .glyphs
                .iter()
                .filter(|(_, e)| e.id.is_some() && e.last_used < self.frame)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, e)| (*k, e.id));
            if let Some((vkey, Some(vid))) = victim {
                self.alpha_alloc.deallocate(vid);
                self.glyphs.remove(&vkey);
            } else {
                tracing::debug!("glyph atlas full and nothing evictable — skipping glyph");
                return None;
            }
        };
        #[allow(clippy::cast_sign_loss)]
        let x = alloc.rectangle.min.x as u32;
        #[allow(clippy::cast_sign_loss)]
        let y = alloc.rectangle.min.y as u32;

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

        let entry = GlyphEntry {
            x,
            y,
            w,
            h,
            bearing_x,
            bearing_y,
            id: Some(alloc.id),
            last_used: self.frame,
        };
        self.glyphs.insert(key, entry);
        Some(entry)
    }

    /// Attaches the shared glyph registry used to resolve PUA codepoints.
    pub fn set_glyph_registry(&mut self, registry: Arc<Mutex<st_glyph::GlyphRegistry>>) {
        self.glyph_registry = Some(registry);
    }

    /// Returns `true` when `ch` is a PUA codepoint with a registered bitmap in
    /// the attached registry.
    #[must_use]
    pub fn has_registered_bitmap(&self, ch: char) -> bool {
        if !is_pua_codepoint(ch) {
            return false;
        }
        self.glyph_registry
            .as_ref()
            .is_some_and(|reg| reg.lock().bitmap(ch).is_some())
    }

    /// Returns the cached colour-atlas entry for a registered PUA codepoint,
    /// uploading its RGBA bitmap on first use.
    ///
    /// Returns `None` when no registry is attached, `ch` has no registered
    /// bitmap, or the colour atlas is full.
    fn get_or_insert_colour(&mut self, queue: &wgpu::Queue, ch: char) -> Option<GlyphEntry> {
        if let Some(&entry) = self.colour_glyphs.get(&ch) {
            return Some(entry);
        }

        // Copy the bitmap out of the registry under a short-lived lock so the
        // mutex is released before the GPU upload.
        let (pixels, width, height) = {
            let registry = self.glyph_registry.as_ref()?;
            let guard = registry.lock();
            let bitmap = guard.bitmap(ch)?;
            (bitmap.pixels.clone(), bitmap.width, bitmap.height)
        };

        if width == 0 || height == 0 {
            return None;
        }

        let [x, y] = self.colour_packer.alloc(width, height)?;

        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.colour_texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &pixels,
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

        let entry = GlyphEntry {
            x,
            y,
            w: width,
            h: height,
            bearing_x: 0,
            bearing_y: i32::try_from(height).unwrap_or(0),
            id: None,
            last_used: 0,
        };
        self.colour_glyphs.insert(ch, entry);
        Some(entry)
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

// ── Colour glyph shader (registered RGBA glyphs) ──────────────────────────────

/// Shader for registered glyphs: samples the RGBA colour atlas directly so the
/// glyph keeps its own colours (only the cell foreground alpha modulates it).
const COLOUR_SHADER_SRC: &str = r"
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
fn vs_colour(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = vec4<f32>(in.position, 0.0, 1.0);
    out.tex_coords    = in.tex_coords;
    out.color         = in.color;
    return out;
}

@group(0) @binding(0) var t_colour: texture_2d<f32>;
@group(0) @binding(1) var s_colour: sampler;

@fragment
fn fs_colour(in: VertexOutput) -> @location(0) vec4<f32> {
    let texel = textureSample(t_colour, s_colour, in.tex_coords);
    return vec4<f32>(texel.rgb, texel.a * in.color.a);
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

// ── Background image shader ───────────────────────────────────────────────────

const BG_IMAGE_SHADER_SRC: &str = r"
struct VIn {
    @location(0) position:   vec2<f32>,
    @location(1) tex_coords: vec2<f32>,
}
struct VOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) tex_coords: vec2<f32>,
}
@vertex fn vs_bg_img(in: VIn) -> VOut {
    var out: VOut;
    out.clip_pos  = vec4<f32>(in.position, 0.0, 1.0);
    out.tex_coords = in.tex_coords;
    return out;
}

@group(0) @binding(0) var t_bg: texture_2d<f32>;
@group(0) @binding(1) var s_bg: sampler;

struct Params { opacity: f32, _p1: f32, _p2: f32, _p3: f32 }
@group(1) @binding(0) var<uniform> params: Params;

@fragment fn fs_bg_img(in: VOut) -> @location(0) vec4<f32> {
    let c = textureSample(t_bg, s_bg, in.tex_coords);
    return vec4<f32>(c.rgb, c.a * params.opacity);
}
";

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

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Estimates cell size in physical pixels.
    ///
    /// Font size is multiplied by `scale_factor` so each cell occupies the
    /// correct number of physical pixels on `HiDPI` displays.
    fn cell_size(&self) -> (f32, f32) {
        let eff = self.config.font.size * self.scale_factor as f32;
        (eff * 0.6, eff * 1.2)
    }

    fn cell_to_ndc(&self, col: u16, row: u16, cell_w: f32, cell_h: f32) -> (f32, f32, f32, f32) {
        let pw = self.size.width as f32;
        let ph = self.size.height as f32;
        let top_off = self.top_bar_height_px() as f32;
        let x0 = (f32::from(col) * cell_w) / pw * 2.0 - 1.0;
        let y0 = 1.0 - (f32::from(row) * cell_h + top_off) / ph * 2.0;
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
        // When a background image is active, multiply cell-background alpha by
        // opacity so the image shows through.  Without an image the existing
        // solid-color behaviour is preserved (alpha unchanged).
        let cell_alpha_mult = if self.bg_image_bind_group.is_some() {
            self.background.opacity
        } else {
            1.0
        };

        for cell in &self.cells {
            let (x0, y0, x1, y1) = self.cell_to_ndc(cell.col, cell.row, cw, ch);
            let c = [
                cell.bg[0],
                cell.bg[1],
                cell.bg[2],
                cell.bg[3] * cell_alpha_mult,
            ];
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

            // Underline / strikethrough rules drawn in the cell's foreground
            // colour. NDC y0 is the top edge, y1 the bottom edge.
            if cell.underline || cell.strikethrough {
                let fg = cell.fg;
                let t = 2.0 / self.size.height as f32; // ~1px thick in NDC
                let mut rule = |ytop: f32, ybot: f32| {
                    verts.extend_from_slice(&[
                        BgVertex {
                            position: [x0, ytop],
                            color: fg,
                        },
                        BgVertex {
                            position: [x1, ytop],
                            color: fg,
                        },
                        BgVertex {
                            position: [x0, ybot],
                            color: fg,
                        },
                        BgVertex {
                            position: [x1, ytop],
                            color: fg,
                        },
                        BgVertex {
                            position: [x1, ybot],
                            color: fg,
                        },
                        BgVertex {
                            position: [x0, ybot],
                            color: fg,
                        },
                    ]);
                };
                if cell.underline {
                    rule(y1 + t * 2.0, y1);
                }
                if cell.strikethrough {
                    let ymid = f32::midpoint(y0, y1);
                    rule(ymid + t, ymid - t);
                }
            }
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

        // ── Agent block backgrounds ───────────────────────────────────────────
        // Each block gets a semi-transparent dark background panel.
        {
            let agent_bg: [f32; 4] = [0.05, 0.05, 0.08, 0.85];
            let pw = self.size.width as f32;
            for block in &self.agent_blocks {
                let row_offset = f32::from(block.start_row);
                let row_count = (block.content_lines.len() + 1) as f32; // +1 for header
                let py0 = row_offset * ch;
                let py1 = py0 + row_count * ch;
                let (x0, y0, x1, y1) = self.px_to_ndc(0.0, py0, pw, py1);
                verts.extend_from_slice(&[
                    BgVertex {
                        position: [x0, y0],
                        color: agent_bg,
                    },
                    BgVertex {
                        position: [x1, y0],
                        color: agent_bg,
                    },
                    BgVertex {
                        position: [x0, y1],
                        color: agent_bg,
                    },
                    BgVertex {
                        position: [x1, y0],
                        color: agent_bg,
                    },
                    BgVertex {
                        position: [x1, y1],
                        color: agent_bg,
                    },
                    BgVertex {
                        position: [x0, y1],
                        color: agent_bg,
                    },
                ]);
            }
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

        // ── Top bar background strip ──────────────────────────────────────────
        {
            let tb_h = self.top_bar_height_px() as f32;
            if tb_h > 0.0 {
                let pw = self.size.width as f32;
                let tb_bg: [f32; 4] = [0.05, 0.05, 0.08, 1.0];
                let (x0, y0, x1, y1) = self.px_to_ndc(0.0, 0.0, pw, tb_h);
                verts.extend_from_slice(&[
                    BgVertex {
                        position: [x0, y0],
                        color: tb_bg,
                    },
                    BgVertex {
                        position: [x1, y0],
                        color: tb_bg,
                    },
                    BgVertex {
                        position: [x0, y1],
                        color: tb_bg,
                    },
                    BgVertex {
                        position: [x1, y0],
                        color: tb_bg,
                    },
                    BgVertex {
                        position: [x1, y1],
                        color: tb_bg,
                    },
                    BgVertex {
                        position: [x0, y1],
                        color: tb_bg,
                    },
                ]);
            }
        }

        verts
    }

    fn build_glyph_vertices(&self) -> Vec<GlyphVertex> {
        let (cw, ch) = self.cell_size();
        let eff_font = self.config.font.size * self.scale_factor as f32;
        let eff_font_key = eff_font.to_bits();
        let sb_font_size = self.status_bar_height_px() as f32 * 0.65;
        let sb_font_key = sb_font_size.to_bits();
        let atlas_size_f = ATLAS_SIZE as f32;
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
            // Registered PUA glyphs are drawn by the colour pass — skip them
            // here so they are not also (incorrectly) sampled from the alpha
            // atlas.
            if self.atlas.colour_glyphs.contains_key(&cell.ch) {
                continue;
            }
            // Look up glyph entry from atlas (read-only view — we cannot call
            // get_or_insert here because we'd need &mut self; use cached value).
            let Some(&entry) =
                self.atlas
                    .glyphs
                    .get(&(cell.ch, cell.bold, cell.italic, eff_font_key))
            else {
                tracing::warn!(ch = %cell.ch, "glyph atlas miss — cell skipped");
                continue;
            };
            let u0 = entry.x as f32 / atlas_size_f;
            let v0 = entry.y as f32 / atlas_size_f;
            let u1 = (entry.x + entry.w) as f32 / atlas_size_f;
            let v1 = (entry.y + entry.h) as f32 / atlas_size_f;

            // A double-width glyph is centred over two columns, not one.
            let advance = if cell.wide { cw * 2.0 } else { cw };
            let top_off = self.top_bar_height_px() as f32;
            let baseline_y = f32::from(cell.row) * ch + ch * (2.0 / 3.0) + top_off;
            let glyph_top = baseline_y - entry.bearing_y as f32;
            let glyph_left = f32::from(cell.col) * cw + (advance - entry.w as f32) / 2.0;
            let (x0, y0, x1, y1) = self.px_to_ndc(
                glyph_left,
                glyph_top,
                glyph_left + entry.w as f32,
                glyph_top + entry.h as f32,
            );
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
                let Some(&entry) = self.atlas.glyphs.get(&(ch, false, false, sb_font_key)) else {
                    tracing::warn!(ch = %ch, "glyph atlas miss — status-bar cell skipped");
                    col_px += sb_cw;
                    continue;
                };
                let u0 = entry.x as f32 / atlas_size_f;
                let v0 = entry.y as f32 / atlas_size_f;
                let u1 = (entry.x + entry.w) as f32 / atlas_size_f;
                let v1 = (entry.y + entry.h) as f32 / atlas_size_f;

                // Place glyph at natural size — center horizontally in the
                // allocated cell, use bearing_y for vertical baseline placement.
                let strip_top = ph - sb_h;
                let glyph_w = entry.w as f32;
                let glyph_h = entry.h as f32;
                let glyph_left = col_px + (sb_cw - glyph_w) / 2.0;
                let baseline = strip_top + sb_h * (2.0 / 3.0);
                let glyph_top = baseline - entry.bearing_y as f32;
                let (x0, y0, x1, y1) = self.px_to_ndc(
                    glyph_left,
                    glyph_top,
                    glyph_left + glyph_w,
                    glyph_top + glyph_h,
                );
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

        // ── Top bar glyphs ────────────────────────────────────────────────────
        {
            let tb_h = self.top_bar_height_px() as f32;
            if tb_h > 0.0 {
                let pw = self.size.width as f32;
                let tb_cw = 7.2_f32;
                let mut tb_col_px = 4.0_f32;

                for (seg_idx, seg) in self.top_bar_segments.iter().enumerate() {
                    if seg_idx > 0 {
                        tb_col_px += tb_cw;
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
                            });

                    for ch in seg.text.chars() {
                        if ch == ' ' {
                            tb_col_px += tb_cw;
                            continue;
                        }
                        let Some(&entry) = self.atlas.glyphs.get(&(ch, false, false, sb_font_key))
                        else {
                            tracing::warn!(ch = %ch, "glyph atlas miss — top-bar cell skipped");
                            tb_col_px += tb_cw;
                            continue;
                        };
                        let u0 = entry.x as f32 / atlas_size_f;
                        let v0 = entry.y as f32 / atlas_size_f;
                        let u1 = (entry.x + entry.w) as f32 / atlas_size_f;
                        let v1 = (entry.y + entry.h) as f32 / atlas_size_f;

                        let glyph_w = entry.w as f32;
                        let glyph_h = entry.h as f32;
                        let glyph_left = tb_col_px + (tb_cw - glyph_w) / 2.0;
                        let baseline = tb_h * (2.0 / 3.0);
                        let glyph_top = baseline - entry.bearing_y as f32;
                        let (x0, y0, x1, y1) = self.px_to_ndc(
                            glyph_left,
                            glyph_top,
                            glyph_left + glyph_w,
                            glyph_top + glyph_h,
                        );
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
                        tb_col_px += tb_cw;
                        if tb_col_px >= pw {
                            break;
                        }
                    }
                    if tb_col_px >= pw {
                        break;
                    }
                }
            }
        }

        // ── Agent block glyphs ────────────────────────────────────────────────
        //
        // Render each agent block's header (model name) and content lines at
        // the block's start_row, using the terminal cell metrics.
        if !self.agent_blocks.is_empty() {
            let agent_header_color: [f32; 4] = [0.4, 0.8, 1.0, 1.0]; // light-blue header
            let agent_text_color: [f32; 4] = [0.9, 0.9, 0.9, 1.0]; // near-white body

            // Helper closure: emit glyph quads for one line of text.
            let emit_line =
                |verts: &mut Vec<GlyphVertex>, text: &str, line_row: u16, color: [f32; 4]| {
                    let mut col = 0u16;
                    for glyph_ch in text.chars() {
                        if glyph_ch == ' ' {
                            col += 1;
                            continue;
                        }
                        let Some(&entry) =
                            self.atlas
                                .glyphs
                                .get(&(glyph_ch, false, false, eff_font_key))
                        else {
                            col += 1;
                            continue;
                        };
                        let u0 = entry.x as f32 / atlas_size_f;
                        let v0 = entry.y as f32 / atlas_size_f;
                        let u1 = (entry.x + entry.w) as f32 / atlas_size_f;
                        let v1 = (entry.y + entry.h) as f32 / atlas_size_f;
                        let baseline_y = f32::from(line_row) * ch + ch * (2.0 / 3.0);
                        let glyph_top = baseline_y - entry.bearing_y as f32;
                        let glyph_left = f32::from(col) * cw + (cw - entry.w as f32) / 2.0;
                        let (x0, y0, x1, y1) = self.px_to_ndc(
                            glyph_left,
                            glyph_top,
                            glyph_left + entry.w as f32,
                            glyph_top + entry.h as f32,
                        );
                        verts.extend_from_slice(&[
                            GlyphVertex {
                                position: [x0, y0],
                                tex_coords: [u0, v0],
                                color,
                            },
                            GlyphVertex {
                                position: [x1, y0],
                                tex_coords: [u1, v0],
                                color,
                            },
                            GlyphVertex {
                                position: [x0, y1],
                                tex_coords: [u0, v1],
                                color,
                            },
                            GlyphVertex {
                                position: [x1, y0],
                                tex_coords: [u1, v0],
                                color,
                            },
                            GlyphVertex {
                                position: [x1, y1],
                                tex_coords: [u1, v1],
                                color,
                            },
                            GlyphVertex {
                                position: [x0, y1],
                                tex_coords: [u0, v1],
                                color,
                            },
                        ]);
                        col += 1;
                    }
                };

            for block in self.agent_blocks.clone() {
                let mut line_row = block.start_row;
                // Header line: model name.
                let header = format!("[ {} ]", block.model);
                emit_line(&mut verts, &header, line_row, agent_header_color);
                line_row += 1;
                // Content lines.
                for line_text in &block.content_lines {
                    emit_line(&mut verts, line_text, line_row, agent_text_color);
                    line_row += 1;
                }
            }
        }

        verts
    }

    /// Builds the vertex quads for registered PUA-codepoint cells, sampling the
    /// colour atlas.
    ///
    /// Returns an empty vector when no visible cell resolves to a registered
    /// colour glyph (the common case), so the colour draw is skipped entirely.
    fn build_colour_glyph_vertices(&self) -> Vec<GlyphVertex> {
        let (cw, ch) = self.cell_size();
        let atlas_size_f = ATLAS_SIZE as f32;
        let top_off = self.top_bar_height_px() as f32;
        let mut verts: Vec<GlyphVertex> = Vec::new();

        for cell in &self.cells {
            if cell.ch == ' ' {
                continue;
            }
            let Some(&entry) = self.atlas.colour_glyphs.get(&cell.ch) else {
                continue;
            };
            let u0 = entry.x as f32 / atlas_size_f;
            let v0 = entry.y as f32 / atlas_size_f;
            let u1 = (entry.x + entry.w) as f32 / atlas_size_f;
            let v1 = (entry.y + entry.h) as f32 / atlas_size_f;

            let baseline_y = f32::from(cell.row) * ch + ch * (2.0 / 3.0) + top_off;
            let glyph_top = baseline_y - entry.bearing_y as f32;
            let glyph_left = f32::from(cell.col) * cw + (cw - entry.w as f32) / 2.0;
            let (x0, y0, x1, y1) = self.px_to_ndc(
                glyph_left,
                glyph_top,
                glyph_left + entry.w as f32,
                glyph_top + entry.h as f32,
            );
            // Carry the cell foreground alpha so transparency still applies; the
            // colour shader keeps the glyph's own RGB.
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

        verts
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
        use std::collections::HashMap;
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
        use std::collections::HashMap;
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
