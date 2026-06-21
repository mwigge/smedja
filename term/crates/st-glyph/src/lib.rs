//! `st-glyph` — glyph shaping utilities and Glyph Protocol implementation for smedja.
//!
//! This crate provides:
//! - Basic font metric helpers (`char_advance_width`, `line_height`, `pixel_size_to_grid`)
//! - APC escape-sequence parser for the smedja Glyph Protocol
//! - Glyph registration (PUA codepoint assignment) and built-in glyph definitions
//! - SVG/PNG rasterisation via `tiny-skia` and the `png` crate
//! - Graceful degradation helpers for terminals that do not support APC sequences

use std::collections::HashMap;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

// ─── Font metric helpers ──────────────────────────────────────────────────────

/// Estimates the advance width of a character in pixels for a given font size.
///
/// Uses a simple monospace approximation (0.6 × `font_size`) that is correct for
/// most terminal fonts.  A more accurate implementation would consult the
/// `FontSystem` from `cosmic-text`.
#[must_use]
pub fn char_advance_width(font_size: f32) -> f32 {
    font_size * 0.6
}

/// Estimates the line height for a given font size.
///
/// Returns `font_size × 1.2`.
#[must_use]
pub fn line_height(font_size: f32) -> f32 {
    font_size * 1.2
}

/// Converts a physical pixel size `(width, height)` and font metrics into a
/// `(cols, rows)` grid size.
///
/// Both dimensions are clamped to a minimum of 1.
#[must_use]
pub fn pixel_size_to_grid(width: u32, height: u32, font_size: f32) -> (u16, u16) {
    let cw = char_advance_width(font_size).max(1.0);
    let ch = line_height(font_size).max(1.0);
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let cols = (width as f32 / cw).floor() as u16;
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rows = (height as f32 / ch).floor() as u16;
    (cols.max(1), rows.max(1))
}

// ─── APC sequence parser ──────────────────────────────────────────────────────

/// Raw payload extracted from an APC (`ESC _ … ESC \`) byte sequence.
///
/// The `id` field is the text before the first `;` in the payload.
/// The `data` field is the full raw payload bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct ApcPayload {
    /// Identifier derived from the payload — the text before the first `;`.
    pub id: String,
    /// Full raw payload bytes (everything between the APC introducer and ST).
    pub data: Vec<u8>,
}

/// Parses an APC byte sequence of the form `ESC _ <payload> ESC \`.
///
/// Returns `None` if the input does not start with the APC introducer (`\x1b_`)
/// or does not contain the string terminator (`\x1b\\`).
#[must_use]
pub fn parse_apc(input: &[u8]) -> Option<ApcPayload> {
    // APC introducer: ESC _ (0x1B 0x5F)
    let introducer: &[u8] = b"\x1b_";
    // String Terminator: ESC \ (0x1B 0x5C)
    let st: &[u8] = b"\x1b\\";

    if !input.starts_with(introducer) {
        return None;
    }

    let after_intro = &input[introducer.len()..];

    // Find the ST suffix
    let st_pos = after_intro.windows(st.len()).position(|w| w == st)?;

    let payload = &after_intro[..st_pos];

    // Derive id: text before the first `;`
    let id = payload
        .iter()
        .position(|&b| b == b';')
        .map_or(payload, |pos| &payload[..pos]);

    let id = String::from_utf8_lossy(id).into_owned();

    Some(ApcPayload {
        id,
        data: payload.to_vec(),
    })
}

// ─── Glyph Protocol RFC parser ────────────────────────────────────────────────

/// Image format carried in a Glyph Protocol registration sequence.
#[derive(Debug, Clone, PartialEq)]
pub enum GlyphFormat {
    /// Scalable Vector Graphics.
    Svg,
    /// Portable Network Graphics.
    Png,
}

/// A decoded Glyph Protocol registration from an APC payload.
#[derive(Debug, Clone, PartialEq)]
pub struct GlyphRegistration {
    /// Glyph identifier (e.g. `smedja.tier.local`).
    pub id: String,
    /// Image format.
    pub format: GlyphFormat,
    /// Base64-decoded image bytes.
    pub data: Vec<u8>,
}

/// Parses a `SMEDJA_GLYPH` APC payload into a [`GlyphRegistration`].
///
/// The expected format is:
/// `SMEDJA_GLYPH;id=<id>;format=<svg|png>;data=<base64>`
///
/// Returns `None` if the payload does not start with `SMEDJA_GLYPH`, is
/// missing required fields, has an unknown format, or carries invalid base64.
#[must_use]
pub fn parse_glyph_registration(payload: &[u8]) -> Option<GlyphRegistration> {
    let text = std::str::from_utf8(payload).ok()?;

    // Must start with the protocol tag
    if !text.starts_with("SMEDJA_GLYPH") {
        return None;
    }

    // Parse semicolon-separated key=value pairs (skip the first token which is the tag)
    let mut id: Option<&str> = None;
    let mut format: Option<GlyphFormat> = None;
    let mut data_b64: Option<&str> = None;

    for part in text.split(';').skip(1) {
        if let Some((key, value)) = part.split_once('=') {
            match key {
                "id" => id = Some(value),
                "format" => {
                    format = match value {
                        "svg" => Some(GlyphFormat::Svg),
                        "png" => Some(GlyphFormat::Png),
                        _ => return None,
                    };
                }
                "data" => data_b64 = Some(value),
                _ => {} // unknown fields are ignored
            }
        }
    }

    let id = id?.to_owned();
    let format = format?;
    let data = BASE64.decode(data_b64?).ok()?;

    Some(GlyphRegistration { id, format, data })
}

// ─── PUA registry ─────────────────────────────────────────────────────────────

/// First codepoint in the Unicode Private Use Area block used by this registry.
const PUA_START: u32 = 0xE000;
/// Last codepoint in the Unicode Private Use Area block (inclusive).
const PUA_END: u32 = 0xF8FF;

/// Maps glyph IDs to Unicode Private Use Area codepoints.
///
/// Codepoints are assigned sequentially starting from `U+E000`.  The registry
/// is idempotent: registering the same ID twice returns the same codepoint.
#[derive(Debug, Clone)]
pub struct GlyphRegistry {
    map: HashMap<String, char>,
    next: u32,
}

impl GlyphRegistry {
    /// Creates an empty [`GlyphRegistry`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
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
}

impl Default for GlyphRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── SVG rasterizer ───────────────────────────────────────────────────────────

/// Rasterises SVG data to a square RGBA bitmap of side `size` pixels.
///
/// Full SVG parsing requires `resvg` which is out of scope for this crate.
/// Instead the function creates a solid-colour pixmap, attempting to parse the
/// `fill="…"` attribute from the SVG source for the colour.  The result is
/// always a valid `size × size × 4` byte buffer even when parsing fails (a
/// magenta placeholder is used as the fallback colour).
///
/// Returns `None` only when `size` is zero.
#[must_use]
pub fn rasterize_svg(svg_data: &[u8], size: u32) -> Option<Vec<u8>> {
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;

    // Try to extract a fill colour from the SVG source.
    let colour = extract_svg_fill_color(svg_data).unwrap_or(tiny_skia::Color::from_rgba8(
        0xFF, 0x00, 0xFF, 0xFF, // magenta placeholder
    ));

    pixmap.fill(colour);

    Some(pixmap.data().to_vec())
}

/// Attempts to extract the first `fill="<hex>"` colour from SVG bytes.
fn extract_svg_fill_color(svg_data: &[u8]) -> Option<tiny_skia::Color> {
    let text = std::str::from_utf8(svg_data).ok()?;
    let fill_pos = text.find("fill=\"")?;
    let start = fill_pos + 6; // skip 'fill="'
    let rest = &text[start..];
    let end = rest.find('"')?;
    let hex = &rest[..end];

    parse_hex_color(hex)
}

/// Parses a CSS hex colour string (`#rgb`, `#rrggbb`) into a `tiny_skia::Color`.
fn parse_hex_color(hex: &str) -> Option<tiny_skia::Color> {
    let hex = hex.strip_prefix('#')?;
    let (r, g, b) = match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            (r, g, b)
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            (r, g, b)
        }
        _ => return None,
    };

    Some(tiny_skia::Color::from_rgba8(r, g, b, 0xFF))
}

/// A decoded glyph ready for inclusion in a GPU texture atlas.
#[derive(Debug, Clone, PartialEq)]
pub struct GlyphAtlasEntry {
    /// PUA codepoint associated with this glyph.
    pub codepoint: char,
    /// RGBA pixel data (`width × height × 4` bytes).
    pub pixels: Vec<u8>,
    /// Width of the decoded image in pixels.
    pub width: u32,
    /// Height of the decoded image in pixels.
    pub height: u32,
}

/// Decodes a PNG image into a [`GlyphAtlasEntry`].
///
/// If the PNG uses an RGB colour type (no alpha channel) each pixel is
/// expanded to RGBA by appending a fully-opaque alpha byte (`0xFF`).
///
/// Returns `None` when decoding fails.
#[must_use]
pub fn decode_png(png_data: &[u8]) -> Option<GlyphAtlasEntry> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png_data));
    let mut reader = decoder.read_info().ok()?;

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;

    let width = info.width;
    let height = info.height;

    let pixels = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => {
            let rgb = &buf[..info.buffer_size()];
            let mut rgba = Vec::with_capacity(rgb.len() / 3 * 4);
            for chunk in rgb.chunks_exact(3) {
                rgba.extend_from_slice(chunk);
                rgba.push(0xFF);
            }
            rgba
        }
        _ => return None,
    };

    Some(GlyphAtlasEntry {
        codepoint: '\u{E000}', // placeholder; caller should set this
        pixels,
        width,
        height,
    })
}

// ─── Built-in smedja glyphs ───────────────────────────────────────────────────

/// SVG source for the `smedja.tier.local` glyph (circle).
const SVG_TIER_LOCAL: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><circle cx="8" cy="8" r="7" fill="#4a9eff"/></svg>"##;

/// SVG source for the `smedja.tier.fast` glyph (lightning bolt).
const SVG_TIER_FAST: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><polygon points="9,1 4,9 8,9 7,15 12,7 8,7" fill="#ffcc00"/></svg>"##;

/// SVG source for the `smedja.tier.deep` glyph (brain/cloud).
const SVG_TIER_DEEP: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><ellipse cx="8" cy="7" rx="6" ry="5" fill="#9b59b6"/><rect x="5" y="10" width="6" height="3" rx="1" fill="#9b59b6"/></svg>"##;

/// SVG source for the `smedja.status.ok` glyph (checkmark).
const SVG_STATUS_OK: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><polyline points="2,8 6,12 14,4" stroke="#2ecc71" stroke-width="2.5" fill="none"/></svg>"##;

/// SVG source for the `smedja.status.fail` glyph (X mark).
const SVG_STATUS_FAIL: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><line x1="3" y1="3" x2="13" y2="13" stroke="#e74c3c" stroke-width="2.5"/><line x1="13" y1="3" x2="3" y2="13" stroke="#e74c3c" stroke-width="2.5"/></svg>"##;

/// SVG source for the `smedja.status.pending` glyph (hourglass).
const SVG_STATUS_PENDING: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><polygon points="3,1 13,1 8,8 13,15 3,15 8,8" fill="#f39c12"/></svg>"##;

/// SVG source for the `smedja.task` glyph (clipboard).
const SVG_TASK: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><rect x="2" y="3" width="12" height="12" rx="1" fill="#95a5a6"/><rect x="5" y="1" width="6" height="3" rx="1" fill="#7f8c8d"/></svg>"##;

/// Mapping of built-in glyph IDs to their SVG source strings.
pub static BUILTIN_GLYPHS: &[(&str, &str)] = &[
    ("smedja.tier.local", SVG_TIER_LOCAL),
    ("smedja.tier.fast", SVG_TIER_FAST),
    ("smedja.tier.deep", SVG_TIER_DEEP),
    ("smedja.status.ok", SVG_STATUS_OK),
    ("smedja.status.fail", SVG_STATUS_FAIL),
    ("smedja.status.pending", SVG_STATUS_PENDING),
    ("smedja.task", SVG_TASK),
];

/// Registers all built-in smedja glyphs in `registry`.
///
/// After this call every glyph ID in [`BUILTIN_GLYPHS`] has a PUA codepoint
/// that can be retrieved via [`GlyphRegistry::lookup`].
pub fn register_builtin_glyphs(registry: &mut GlyphRegistry) {
    for (id, _svg) in BUILTIN_GLYPHS {
        registry.register(id);
    }
}

// ─── Graceful degradation ─────────────────────────────────────────────────────

/// Returns `true` when `term` identifies a terminal emulator that supports APC
/// sequences (`WezTerm`, `kitty`, `foot`, `iTerm`).
///
/// The check is case-insensitive and matches on a substring of the `TERM` or
/// `TERM_PROGRAM` environment variable.
#[must_use]
pub fn supports_apc(term: &str) -> bool {
    let lower = term.to_lowercase();
    lower.contains("wezterm")
        || lower.contains("kitty")
        || lower.contains("foot")
        || lower.contains("iterm")
}

/// Returns a plain-text fallback label for terminals that do not render glyphs.
///
/// Unknown glyph IDs map to `"[?]"`.
#[must_use]
pub fn fallback_text(glyph_id: &str) -> &'static str {
    match glyph_id {
        "smedja.tier.local" => "[local]",
        "smedja.tier.fast" => "[fast]",
        "smedja.tier.deep" => "[deep]",
        "smedja.status.ok" => "\u{2713}",      // ✓
        "smedja.status.fail" => "\u{2717}",    // ✗
        "smedja.status.pending" => "\u{23F3}", // ⏳
        "smedja.task" => "[task]",
        _ => "[?]",
    }
}

// ─── Public API for smdjad ────────────────────────────────────────────────────

/// Builds the APC byte sequences that `smdjad` emits on stdout to register
/// all glyphs currently in `registry`.
///
/// Each registered glyph produces one sequence of the form:
/// `ESC _ SMEDJA_GLYPH;id=<id>;codepoint=<hex> ESC \`
///
/// The caller is responsible for writing the returned bytes to stdout when
/// `SMEDJA_TERM_PANE` is set.
#[must_use]
pub fn build_glyph_registration_sequence(registry: &GlyphRegistry) -> Vec<u8> {
    let mut out = Vec::new();

    for (id, cp) in registry.entries() {
        // ESC _
        out.push(0x1B);
        out.push(b'_');

        let payload = format!("SMEDJA_GLYPH;id={id};codepoint={cp:04X}", cp = cp as u32);
        out.extend_from_slice(payload.as_bytes());

        // ESC \
        out.push(0x1B);
        out.push(b'\\');
    }

    out
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Existing metric helper tests

    #[test]
    fn char_advance_width_scales_with_font_size() {
        assert!((char_advance_width(14.0) - 8.4).abs() < 0.01);
    }

    #[test]
    fn line_height_scales_with_font_size() {
        assert!((line_height(14.0) - 16.8).abs() < 0.01);
    }

    #[test]
    fn pixel_size_to_grid_computes_correctly() {
        // 800×600 window at 14pt font → ~95 cols, ~35 rows
        let (cols, rows) = pixel_size_to_grid(800, 600, 14.0);
        assert!(cols > 0);
        assert!(rows > 0);
    }

    #[test]
    fn pixel_size_to_grid_minimum_is_one() {
        let (cols, rows) = pixel_size_to_grid(1, 1, 14.0);
        assert_eq!(cols, 1);
        assert_eq!(rows, 1);
    }

    // APC parser tests

    #[test]
    fn parse_apc_valid_sequence() {
        let input = b"\x1b_hello;world\x1b\\";
        let payload = parse_apc(input).expect("should parse valid APC sequence");
        assert_eq!(payload.id, "hello");
        assert!(!payload.data.is_empty());
    }

    #[test]
    fn parse_apc_invalid_returns_none() {
        assert!(parse_apc(b"not an apc sequence").is_none());
    }

    // Glyph Protocol parser tests

    #[test]
    fn glyph_protocol_parse_svg_registration() {
        let b64 = BASE64.encode(b"hello");
        let payload = format!("SMEDJA_GLYPH;id=test.svg;format=svg;data={b64}");
        let reg = parse_glyph_registration(payload.as_bytes())
            .expect("should parse valid SVG registration");
        assert_eq!(reg.format, GlyphFormat::Svg);
        assert_eq!(reg.id, "test.svg");
        assert_eq!(reg.data, b"hello");
    }

    #[test]
    fn glyph_protocol_parse_png_registration() {
        let b64 = BASE64.encode(b"pngbytes");
        let payload = format!("SMEDJA_GLYPH;id=test.png;format=png;data={b64}");
        let reg = parse_glyph_registration(payload.as_bytes())
            .expect("should parse valid PNG registration");
        assert_eq!(reg.format, GlyphFormat::Png);
        assert_eq!(reg.id, "test.png");
        assert_eq!(reg.data, b"pngbytes");
    }

    // PUA registry tests

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

    // Built-in glyphs test

    #[test]
    fn builtin_glyphs_all_register_without_panic() {
        let mut reg = GlyphRegistry::new();
        register_builtin_glyphs(&mut reg);
        for (id, _) in BUILTIN_GLYPHS {
            assert!(
                reg.lookup(id).is_some(),
                "expected glyph '{id}' to be registered"
            );
        }
    }

    // Graceful degradation tests

    #[test]
    fn supports_apc_wezterm_returns_true() {
        assert!(supports_apc("xterm-256color-wezterm"));
    }

    #[test]
    fn supports_apc_xterm_returns_false() {
        assert!(!supports_apc("xterm-256color"));
    }

    #[test]
    fn fallback_text_maps_tier_local() {
        assert_eq!(fallback_text("smedja.tier.local"), "[local]");
    }
}
