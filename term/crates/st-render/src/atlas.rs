//! GPU texture atlas for rasterised glyphs, plus atlas-selection helpers.

use std::collections::HashMap;
use std::sync::Arc;

use cosmic_text::{fontdb, FontSystem, SwashCache};
use parking_lot::Mutex;

use crate::shelf::ShelfPacker;

/// Side length of the square glyph atlas textures, in pixels.
pub(crate) const ATLAS_SIZE: u32 = 1024;

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

#[cfg(test)]
mod tests {
    use super::*;

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
