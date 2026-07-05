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

use std::fmt::Write as _;

mod pipeline;
mod registry;
mod strategies;
#[cfg(test)]
mod tests;

pub use pipeline::{
    code_compressor, command_compressor, smart_crusher, ContentPipeline, Transform,
};
pub use registry::{FilterEntry, FilterParams, FilterRegistry, FilterStrategy};
pub use strategies::{dedup_lines, group_by_directory, smart_filter, truncate_lines};

// ── Bypass helper ────────────────────────────────────────────────────────────

/// Returns `true` when `SMEDJA_NO_TOOL_COMPRESS` is set to `1`.
fn bypass_enabled() -> bool {
    std::env::var("SMEDJA_NO_TOOL_COMPRESS").as_deref() == Ok("1")
}

// ── Task 51 — SmartCrusher ───────────────────────────────────────────────────

/// Strips JSON null and empty-array fields recursively from a serialised JSON string.
///
/// Non-JSON input is returned unchanged.  Honouring `SMEDJA_NO_TOOL_COMPRESS=1`
/// bypasses all processing and returns the content as-is.
#[must_use]
pub fn compress_tool_result(content: &str) -> String {
    if bypass_enabled() {
        return content.to_owned();
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return content.to_owned();
    };

    let stripped = strip_nulls_and_empty_arrays(value);
    serde_json::to_string(&stripped).unwrap_or_else(|_| content.to_owned())
}

/// Recursively removes all JSON null and empty-array fields from an object or array.
fn strip_nulls_and_empty_arrays(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let filtered = map
                .into_iter()
                .filter(|(_, v)| {
                    !v.is_null() && !matches!(v, serde_json::Value::Array(arr) if arr.is_empty())
                })
                .map(|(k, v)| (k, strip_nulls_and_empty_arrays(v)))
                .collect();
            serde_json::Value::Object(filtered)
        }
        serde_json::Value::Array(arr) => {
            let filtered = arr.into_iter().map(strip_nulls_and_empty_arrays).collect();
            serde_json::Value::Array(filtered)
        }
        other => other,
    }
}

// ── Task 52 — RTK-style command-aware compressor ─────────────────────────────

/// Compresses command output by dispatching through the default
/// [`FilterRegistry`] keyed on the detected command.
///
/// Returns `(compressed_output, ratio)` where `ratio = compressed.len() as f32 /
/// output.len() as f32`.  A ratio below 1.0 means the output was reduced.
///
/// The strategy is selected by [`FilterRegistry::with_defaults`] from the first
/// one or two tokens of `cmd`.  The default set preserves the historical
/// `cargo test` (smart-filter) and `git status` (group) behaviour; an
/// unrecognised command falls back to the conservative blank-line removal.
///
/// `SMEDJA_NO_TOOL_COMPRESS=1` bypasses all processing and returns the output
/// verbatim with ratio `1.0`.
#[must_use]
pub fn compress_command_output(cmd: &str, output: &str) -> (String, f32) {
    compress_command_output_with(&FilterRegistry::with_defaults(), cmd, output)
}

/// Compresses command output using an explicit `registry`.
///
/// This is the registry-aware core of [`compress_command_output`]; callers that
/// have loaded a user `.smedja/filters.toml` registry route through here so the
/// merged user/default filter set is applied.  The bypass env var and the
/// empty-output shortcut are honoured identically.
///
/// Returns `(compressed_output, ratio)`.
#[must_use]
pub fn compress_command_output_with(
    registry: &FilterRegistry,
    cmd: &str,
    output: &str,
) -> (String, f32) {
    if bypass_enabled() {
        return (output.to_owned(), 1.0_f32);
    }

    if output.is_empty() {
        return (String::new(), 1.0_f32);
    }

    let (strategy, params) = registry.resolve(cmd);
    let compressed = strategy.apply(output, &params);

    #[allow(clippy::cast_precision_loss)] // advisory ratio; precision loss is acceptable
    let ratio = compressed.len() as f32 / output.len() as f32;
    (compressed, ratio)
}

// ── Task 53 — CodeCompressor ─────────────────────────────────────────────────

/// Truncates a code block body that exceeds 80 lines.
///
/// When the block exceeds the threshold the first 20 lines are kept, followed
/// by a comment indicating the number of omitted lines.
///
/// The `lang` parameter must be non-empty for truncation to apply.  Blocks with
/// an empty `lang` string are returned unchanged (e.g. plain text blocks).
///
/// `SMEDJA_NO_TOOL_COMPRESS=1` is **not** honoured here — code block trimming
/// is independent of tool-result compression.
#[must_use]
pub fn trim_code_block(lang: &str, body: &str) -> String {
    const THRESHOLD: usize = 80;
    const KEEP: usize = 20;

    if lang.is_empty() {
        return body.to_owned();
    }

    let lines: Vec<&str> = body.lines().collect();
    if lines.len() <= THRESHOLD {
        return body.to_owned();
    }

    let omitted = lines.len() - KEEP;
    let mut out = lines[..KEEP].join("\n");
    out.push('\n');
    let _ = write!(
        out,
        "// … {omitted} lines omitted (smedja_retrieve to expand)"
    );
    out
}
