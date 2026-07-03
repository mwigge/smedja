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

mod atlas;
mod background;
mod cell;
mod error;
mod renderer;
mod shader;
mod shelf;
mod vertex;

// Re-export so callers don't have to depend on winit/wgpu directly.
pub use wgpu;
pub use winit;

pub use atlas::{is_pua_codepoint, select_atlas, AtlasKind, GlyphAtlas, GlyphEntry};
pub use background::BackgroundConfig;
pub use cell::{AgentBlockView, BlockDecoration, Cell};
pub use error::RenderError;
pub use renderer::Renderer;
pub use shelf::ShelfPacker;
pub use vertex::{BgImageVertex, BgVertex, GlyphVertex};
