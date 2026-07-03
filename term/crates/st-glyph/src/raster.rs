//! SVG and PNG rasterisation into GPU-atlas-ready RGBA bitmaps.

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
