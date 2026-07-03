//! `st-glyph` — glyph shaping utilities and Glyph Protocol implementation for smedja.
//!
//! This crate provides:
//! - Basic font metric helpers (`char_advance_width`, `line_height`, `pixel_size_to_grid`)
//! - APC escape-sequence parser for the smedja Glyph Protocol
//! - Glyph registration (PUA codepoint assignment) and built-in glyph definitions
//! - SVG/PNG rasterisation via `tiny-skia` and the `png` crate
//! - Graceful degradation helpers for terminals that do not support APC sequences

mod badges;
mod builtins;
mod metrics;
mod protocol;
mod raster;
mod registry;

pub use badges::{
    fallback_text, glyph_id_for_status, glyph_id_for_tier, resolve_badge, supports_apc, BadgeRender,
};
pub use builtins::{register_builtin_glyphs, BUILTIN_GLYPHS};
pub use metrics::{char_advance_width, line_height, pixel_size_to_grid};
pub use protocol::{
    build_glyph_registration_sequence, parse_apc, parse_glyph_registration, ApcPayload,
    GlyphFormat, GlyphRegistration,
};
pub use raster::{decode_png, rasterize_svg, GlyphAtlasEntry};
pub use registry::GlyphRegistry;
