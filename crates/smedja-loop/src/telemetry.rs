//! OpenTelemetry instrumentation for the loop engine.
//!
//! Follows the `smedja.*` attribute namespace and the
//! `smedja_loop_*` metric naming convention.
//!
//! Prompt text is captured only when `SMEDJA_CAPTURE_PROMPTS=1` is set in the
//! environment.  The prompt hash is always emitted so prompts can be
//! correlated across runs without storing sensitive content.

use opentelemetry::{global, KeyValue};
use sha2::{Digest, Sha256};

/// Computes the SHA-256 hash of `text` and returns it as a lowercase hex string.
///
/// Used to fingerprint prompt content without storing the raw text.
#[must_use]
pub fn prompt_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    format!("{:x}", h.finalize())
}

/// Emits a `smedja.loop.prompt` `OTel` span carrying prompt metadata.
///
/// Attributes emitted:
/// - `smedja.loop.prompt_hash` — always present
/// - `smedja.loop.prompt_tokens` — estimated token count (or `0`)
/// - `smedja.loop.prompt_text` — only when `SMEDJA_CAPTURE_PROMPTS=1`
pub fn emit_prompt_span(tracer: &impl opentelemetry::trace::Tracer, prompt: &str, tokens: i64) {
    use opentelemetry::trace::Span;

    let hash = prompt_hash(prompt);
    let mut span = tracer.start("smedja.loop.prompt");
    span.set_attribute(KeyValue::new("smedja.loop.prompt_hash", hash));
    span.set_attribute(KeyValue::new("smedja.loop.prompt_tokens", tokens));
    if std::env::var("SMEDJA_CAPTURE_PROMPTS").as_deref() == Ok("1") {
        span.set_attribute(KeyValue::new("smedja.loop.prompt_text", prompt.to_owned()));
    }
    span.end();
}

/// Records the standard per-role span attributes onto `span`.
///
/// Attributes set:
/// - `smedja.loop.role`
/// - `smedja.loop.runner`
/// - `smedja.loop.tier`
/// - `smedja.loop.attempt`
pub fn set_role_attributes(
    span: &mut impl opentelemetry::trace::Span,
    role: &str,
    runner: &str,
    tier: &str,
    attempt: u32,
) {
    span.set_attribute(KeyValue::new("smedja.loop.role", role.to_owned()));
    span.set_attribute(KeyValue::new("smedja.loop.runner", runner.to_owned()));
    span.set_attribute(KeyValue::new("smedja.loop.tier", tier.to_owned()));
    span.set_attribute(KeyValue::new("smedja.loop.attempt", i64::from(attempt)));
}

/// Records loop-level counters using the global `OTel` meter.
///
/// Labels applied to every metric: `change_name`, `status`.
///
/// Metrics recorded:
/// - `smedja_loop_slices_total`
/// - `smedja_loop_input_tokens_total`
/// - `smedja_loop_output_tokens_total`
pub fn record_loop_metrics(
    change_name: &str,
    status: &str,
    slices: u64,
    input_tok: u64,
    output_tok: u64,
) {
    let meter = global::meter("smedja.loop");
    let labels = [
        KeyValue::new("change_name", change_name.to_owned()),
        KeyValue::new("status", status.to_owned()),
    ];
    meter
        .u64_counter("smedja_loop_slices_total")
        .build()
        .add(slices, &labels);
    meter
        .u64_counter("smedja_loop_input_tokens_total")
        .build()
        .add(input_tok, &labels);
    meter
        .u64_counter("smedja_loop_output_tokens_total")
        .build()
        .add(output_tok, &labels);
}

/// Records tier-escalation and token-compression counters.
///
/// Labels applied: `change_name`, `status`.
///
/// Metrics recorded:
/// - `smedja_loop_tier_escalations_total`
/// - `smedja_loop_tokens_compressed_total`
pub fn record_escalation_metrics(
    change_name: &str,
    status: &str,
    escalations: u64,
    tokens_compressed: u64,
) {
    let meter = global::meter("smedja.loop");
    let labels = [
        KeyValue::new("change_name", change_name.to_owned()),
        KeyValue::new("status", status.to_owned()),
    ];
    meter
        .u64_counter("smedja_loop_tier_escalations_total")
        .build()
        .add(escalations, &labels);
    meter
        .u64_counter("smedja_loop_tokens_compressed_total")
        .build()
        .add(tokens_compressed, &labels);
}

/// Records the loop duration histogram.
///
/// Labels applied: `change_name`, `status`.
///
/// Metric recorded:
/// - `smedja_loop_duration_seconds`
pub fn record_loop_duration(change_name: &str, status: &str, duration_secs: f64) {
    let meter = global::meter("smedja.loop");
    let labels = [
        KeyValue::new("change_name", change_name.to_owned()),
        KeyValue::new("status", status.to_owned()),
    ];
    meter
        .f64_histogram("smedja_loop_duration_seconds")
        .build()
        .record(duration_secs, &labels);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_hash_is_deterministic() {
        let h1 = prompt_hash("hello world");
        let h2 = prompt_hash("hello world");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64, "SHA-256 hex must be 64 chars");
    }

    #[test]
    fn prompt_hash_differs_for_different_inputs() {
        let h1 = prompt_hash("input one");
        let h2 = prompt_hash("input two");
        assert_ne!(h1, h2);
    }

    #[test]
    fn prompt_hash_of_empty_string_is_stable() {
        // SHA-256("") is a well-known constant.
        let expected = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(prompt_hash(""), expected);
    }
}
