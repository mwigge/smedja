//! Context-window compression transforms.
//!
//! This module provides three compressors that shrink context content before
//! it is serialised into an outbound LLM request:
//!
//! - [`compress_tool_result`] — strips JSON null and empty-array fields recursively.
//! - [`compress_command_output`] — removes known-noisy lines per command type.
//! - [`trim_code_block`] — truncates long code blocks to first 20 lines.
//!
//! Each function honours the `SMEDJA_NO_TOOL_COMPRESS=1` environment variable
//! as a bypass.
//!
//! The [`ContentPipeline`] struct chains arbitrary transform closures and
//! applies them in sequence.

mod code;
mod command;
mod crusher;
mod filters;
mod pipeline;
mod registry;

pub use code::trim_code_block;
pub use command::{compress_command_output, compress_command_output_with};
pub use crusher::compress_tool_result;
pub use filters::{dedup_lines, group_by_directory, smart_filter, truncate_lines};
pub use pipeline::{
    code_compressor, command_compressor, smart_crusher, ContentPipeline, Transform,
};
pub use registry::{FilterEntry, FilterParams, FilterRegistry, FilterStrategy};

// ── Bypass helper ────────────────────────────────────────────────────────────

/// Returns `true` when `SMEDJA_NO_TOOL_COMPRESS` is set to `1`.
pub(crate) fn bypass_enabled() -> bool {
    std::env::var("SMEDJA_NO_TOOL_COMPRESS").as_deref() == Ok("1")
}
