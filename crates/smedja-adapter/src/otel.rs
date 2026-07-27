//! Shared `OTel` utilities for HTTP adapters.

use std::collections::HashMap;
use std::sync::OnceLock;

use opentelemetry::metrics::Histogram;
use opentelemetry::KeyValue;

/// Injects a W3C `traceparent` header into `headers` using the current `OTel` context.
///
/// If no propagator has been installed (e.g. in tests without `OTel` setup), the
/// function is a no-op.  If the current span context is invalid (background
/// context), the propagator will not emit a `traceparent` value, so no header is
/// added.
pub(crate) fn inject_traceparent(headers: &mut reqwest::header::HeaderMap) {
    let cx = opentelemetry::Context::current();
    let mut map: HashMap<String, String> = HashMap::new();
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut map);
    });
    for (k, v) in &map {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            headers.insert(name, value);
        }
    }
}

/// The `gen_ai.client.token.usage` histogram (GenAI semconv), shared lazily by
/// all providers. Built from the global meter — noop until smdjad installs a
/// meter provider, so recording is always safe.
fn token_usage_histogram() -> &'static Histogram<u64> {
    static HISTOGRAM: OnceLock<Histogram<u64>> = OnceLock::new();
    HISTOGRAM.get_or_init(|| {
        opentelemetry::global::meter("smedja")
            .u64_histogram("gen_ai.client.token.usage")
            .with_description("Number of input and output tokens used per LLM call")
            .with_unit("{token}")
            .build()
    })
}

/// Records one LLM call's token usage as two data points (input + output) on
/// the `gen_ai.client.token.usage` histogram, keyed by provider system, model,
/// and token type. Zero values are skipped — an absent usage report must not
/// pollute the distribution.
pub(crate) fn record_token_usage(system: &str, model: &str, input: u64, output: u64) {
    let histogram = token_usage_histogram();
    for (token_type, value) in [("input", input), ("output", output)] {
        if value == 0 {
            continue;
        }
        histogram.record(
            value,
            &[
                KeyValue::new("gen_ai.system", system.to_owned()),
                KeyValue::new("gen_ai.request.model", model.to_owned()),
                KeyValue::new("gen_ai.operation.name", "chat"),
                KeyValue::new("gen_ai.token.type", token_type),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_token_usage_is_safe_under_noop_meter() {
        // No meter provider installed in tests → records into the noop meter.
        record_token_usage("anthropic", "claude-test", 100, 50);
        record_token_usage("openai", "gpt-test", 0, 0);
    }
}
