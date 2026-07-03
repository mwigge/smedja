//! APC escape-sequence parser and Glyph Protocol registration codec.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use crate::registry::GlyphRegistry;

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

/// Image format carried in a Glyph Protocol registration sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
