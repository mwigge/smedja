//! Shared `OTel` utilities for HTTP adapters.

use std::collections::HashMap;

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
