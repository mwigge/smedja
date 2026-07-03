//! PUA codepoint registry mapping glyph IDs to codepoints and cached bitmaps.

use std::collections::HashMap;

use crate::protocol::GlyphFormat;
use crate::raster::{decode_png, rasterize_svg, GlyphAtlasEntry};

/// First codepoint in the Unicode Private Use Area block used by this registry.
const PUA_START: u32 = 0xE000;
/// Last codepoint in the Unicode Private Use Area block (inclusive).
const PUA_END: u32 = 0xF8FF;

/// Fixed side length (pixels) at which SVG glyphs are rasterised on registration.
const SVG_REGISTRATION_SIZE: u32 = 32;

/// Maps glyph IDs to Unicode Private Use Area codepoints.
///
/// Codepoints are assigned sequentially starting from `U+E000`.  The registry
/// is idempotent: registering the same ID twice returns the same codepoint.
#[derive(Debug, Clone)]
pub struct GlyphRegistry {
    map: HashMap<String, char>,
    bitmaps: HashMap<char, GlyphAtlasEntry>,
    next: u32,
}

impl GlyphRegistry {
    /// Creates an empty [`GlyphRegistry`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            bitmaps: HashMap::new(),
            next: PUA_START,
        }
    }

    /// Assigns a PUA codepoint to `id` and returns it.
    ///
    /// If `id` is already registered the existing codepoint is returned without
    /// consuming a new slot (idempotent).  If the PUA range is exhausted a
    /// warning is emitted and the last valid codepoint (`U+F8FF`) is returned.
    ///
    /// # Panics
    ///
    /// This function does not panic in practice.  The internal `expect` calls
    /// guard against `char::from_u32` returning `None`, which cannot happen
    /// because both `PUA_START..=PUA_END` and the literal `U+F8FF` are
    /// statically-known valid Unicode scalar values.
    pub fn register(&mut self, id: &str) -> char {
        if let Some(&cp) = self.map.get(id) {
            return cp;
        }

        let cp = if self.next > PUA_END {
            tracing::warn!(
                glyph_id = id,
                "PUA range exhausted; reusing U+F8FF for glyph"
            );
            // SAFETY: U+F8FF is a valid Unicode scalar value (it is in the PUA block).
            char::from_u32(PUA_END).expect("PUA_END is always a valid char")
        } else {
            // SAFETY: every value in PUA_START..=PUA_END is a valid Unicode scalar value.
            let c = char::from_u32(self.next).expect("PUA codepoint is always valid");
            self.next += 1;
            c
        };

        self.map.insert(id.to_owned(), cp);
        cp
    }

    /// Looks up the PUA codepoint assigned to `id`.
    ///
    /// Returns `None` if `id` has not been registered.
    #[must_use]
    pub fn lookup(&self, id: &str) -> Option<char> {
        self.map.get(id).copied()
    }

    /// Returns an iterator over all registered `(id, codepoint)` pairs.
    pub fn entries(&self) -> impl Iterator<Item = (&str, char)> {
        self.map.iter().map(|(k, &v)| (k.as_str(), v))
    }

    /// Registers `id`, rasterises its shape, and caches the resulting bitmap
    /// keyed by the assigned PUA codepoint.
    ///
    /// The codepoint is assigned (or reused for an already-registered `id`)
    /// exactly as in [`Self::register`].  The `data` bytes are rasterised
    /// according to `format` ([`rasterize_svg`] for SVG, [`decode_png`] for
    /// PNG) and the resulting [`GlyphAtlasEntry`] — with its `codepoint` field
    /// set to the assigned codepoint — replaces any previously-stored bitmap.
    ///
    /// Rasterisation failure (undecodable PNG, zero-size SVG) leaves the
    /// id→codepoint mapping in place with **no** bitmap, so [`Self::lookup`]
    /// still resolves and the renderer falls back to plain text or tofu.
    ///
    /// Returns the assigned codepoint.
    pub fn register_shape(&mut self, id: &str, format: GlyphFormat, data: &[u8]) -> char {
        let cp = self.register(id);

        let entry = match format {
            GlyphFormat::Svg => {
                rasterize_svg(data, SVG_REGISTRATION_SIZE).map(|pixels| GlyphAtlasEntry {
                    codepoint: cp,
                    pixels,
                    width: SVG_REGISTRATION_SIZE,
                    height: SVG_REGISTRATION_SIZE,
                })
            }
            GlyphFormat::Png => decode_png(data).map(|mut entry| {
                entry.codepoint = cp;
                entry
            }),
        };

        if let Some(entry) = entry {
            self.bitmaps.insert(cp, entry);
        } else {
            tracing::warn!(
                glyph_id = id,
                "glyph rasterisation failed; keeping mapping without a bitmap"
            );
            // Drop any stale bitmap so a failed re-registration does not
            // leave the previous shape resolving under this codepoint.
            self.bitmaps.remove(&cp);
        }

        cp
    }

    /// Looks up the rasterised bitmap for a PUA codepoint.
    ///
    /// Returns `None` if no shape has been registered for `cp` (id-only
    /// registration via [`Self::register`], or a rasterisation failure).
    #[must_use]
    pub fn bitmap(&self, cp: char) -> Option<&GlyphAtlasEntry> {
        self.bitmaps.get(&cp)
    }
}

impl Default for GlyphRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pua_registry_assigns_sequential_codepoints() {
        let mut reg = GlyphRegistry::new();
        let a = reg.register("a");
        let b = reg.register("b");
        assert_eq!(a, '\u{E000}');
        assert_eq!(b, '\u{E001}');
    }

    #[test]
    fn pua_registry_collision_detection() {
        let mut reg = GlyphRegistry::new();
        let first = reg.register("same");
        let second = reg.register("same");
        assert_eq!(first, second);
    }

    /// Builds a minimal valid 1×1 RGB PNG (no alpha) for rasterisation tests.
    fn one_by_one_png() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            writer
                .write_image_data(&[0x12, 0x34, 0x56])
                .expect("png data");
        }
        out
    }

    #[test]
    fn register_shape_stores_bitmap_keyed_by_codepoint() {
        let mut reg = GlyphRegistry::new();
        let png = one_by_one_png();
        let cp = reg.register_shape("test.png", GlyphFormat::Png, &png);
        let entry = reg
            .bitmap(cp)
            .expect("registered PNG should have a stored bitmap");
        assert_eq!(entry.codepoint, cp);
        assert_eq!(entry.width, 1);
        assert_eq!(entry.height, 1);
    }

    #[test]
    fn register_shape_is_idempotent_and_replaces_bitmap() {
        let mut reg = GlyphRegistry::new();
        let png = one_by_one_png();
        let first = reg.register_shape("dup", GlyphFormat::Png, &png);
        // Re-register the same id with an SVG shape; codepoint must be stable.
        let svg = br##"<svg fill="#ff0000"></svg>"##;
        let second = reg.register_shape("dup", GlyphFormat::Svg, svg);
        assert_eq!(first, second, "re-registering an id keeps its codepoint");
        let entry = reg.bitmap(second).expect("bitmap should be present");
        // The SVG rasterises to a 32×32 RGBA square, replacing the 1×1 PNG.
        assert_eq!(entry.width, 32);
        assert_eq!(entry.height, 32);
    }

    #[test]
    fn register_shape_rasterise_failure_keeps_mapping_without_bitmap() {
        let mut reg = GlyphRegistry::new();
        let cp = reg.register_shape("bad.png", GlyphFormat::Png, b"not a png");
        assert_eq!(
            reg.lookup("bad.png"),
            Some(cp),
            "mapping must survive rasterisation failure"
        );
        assert!(
            reg.bitmap(cp).is_none(),
            "undecodable PNG must leave no bitmap"
        );
    }
}
