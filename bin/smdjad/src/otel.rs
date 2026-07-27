//! `OTel` pipeline wiring for smdjad: traces, metrics, and logs over OTLP/HTTP.
//!
//! Everything here is gated on `SMEDJA_OTLP_ENDPOINT`; unset, the daemon runs
//! with the noop providers and only its structured `tracing` logs. The endpoint
//! is the bare collector address (e.g. `http://localhost:4318`); this module
//! appends the per-signal path (`/v1/traces`, `/v1/metrics`, `/v1/logs`)
//! itself — the programmatic `with_endpoint` route of opentelemetry-otlp 0.27
//! uses the given URL verbatim, unlike the `OTEL_EXPORTER_OTLP_ENDPOINT` env
//! route which appends the path.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry_sdk::logs::LoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::Resource;
use tracing::{error, info};

/// Environment variable gating the whole OTLP pipeline.
pub(crate) const ENV_OTLP_ENDPOINT: &str = "SMEDJA_OTLP_ENDPOINT";

/// The resource identity attached to every exported span, metric, and log
/// record — without it the backend groups everything under `unknown_service`.
fn resource() -> Resource {
    Resource::new_with_defaults([
        KeyValue::new("service.name", "smdjad"),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
    ])
}

/// Reads `SMEDJA_OTLP_ENDPOINT`, treating an empty value as unset.
pub(crate) fn otlp_endpoint() -> Option<String> {
    std::env::var(ENV_OTLP_ENDPOINT)
        .ok()
        .filter(|v| !v.is_empty())
}

/// Joins the bare collector address with a per-signal path, tolerating a
/// trailing slash in the configured endpoint.
fn signal_endpoint(endpoint: &str, signal_path: &str) -> String {
    format!("{}{}", endpoint.trim_end_matches('/'), signal_path)
}

/// Installs the global tracer provider with a batched OTLP/HTTP span exporter.
///
/// Returns `false` when the exporter failed to build — the daemon then keeps
/// the noop provider and structured logs remain the only trace destination
/// (mirroring the pre-pipeline behaviour, never a silent discard).
pub(crate) fn install_traces(endpoint: &str) -> bool {
    use opentelemetry_otlp::WithExportConfig as _;
    let build_result = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(signal_endpoint(endpoint, "/v1/traces"))
        .build();
    match build_result {
        Ok(exporter) => {
            let provider = opentelemetry_sdk::trace::TracerProvider::builder()
                .with_resource(resource())
                .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
                .build();
            opentelemetry::global::set_tracer_provider(provider);
            info!(endpoint = %endpoint, "otel traces: OTLP exporter installed");
            true
        }
        Err(e) => {
            error!(error = %e, endpoint = %endpoint, "otel traces: failed to build OTLP exporter; structured logs only");
            false
        }
    }
}

/// Installs the global meter provider with a periodic OTLP/HTTP metric
/// exporter. The 15 s read cadence (vs. the 60 s default) keeps local
/// backends like SigNoz responsive enough for live demos.
pub(crate) fn install_metrics(endpoint: &str) -> bool {
    use opentelemetry_otlp::WithExportConfig as _;
    let build_result = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_endpoint(signal_endpoint(endpoint, "/v1/metrics"))
        .build();
    match build_result {
        Ok(exporter) => {
            let reader = PeriodicReader::builder(exporter, opentelemetry_sdk::runtime::Tokio)
                .with_interval(std::time::Duration::from_secs(15))
                .build();
            let provider = SdkMeterProvider::builder()
                .with_resource(resource())
                .with_reader(reader)
                .build();
            opentelemetry::global::set_meter_provider(provider);
            info!(endpoint = %endpoint, "otel metrics: OTLP exporter installed");
            true
        }
        Err(e) => {
            error!(error = %e, endpoint = %endpoint, "otel metrics: failed to build OTLP exporter");
            false
        }
    }
}

/// Builds the logger provider backing the `tracing` → OTLP log bridge.
///
/// Built before the subscriber exists (the bridge is a subscriber layer), so
/// a build failure is reported on stderr rather than through `tracing`.
pub(crate) fn build_logger_provider(endpoint: &str) -> Option<LoggerProvider> {
    use opentelemetry_otlp::WithExportConfig as _;
    match opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_endpoint(signal_endpoint(endpoint, "/v1/logs"))
        .build()
    {
        Ok(exporter) => Some(
            LoggerProvider::builder()
                .with_resource(resource())
                .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
                .build(),
        ),
        Err(e) => {
            eprintln!("otel logs: failed to build OTLP exporter for {endpoint}: {e}");
            None
        }
    }
}

/// Counts JSON-RPC requests by method as `smedja_rpc_requests_total`.
///
/// Recorded at the router choke point so every registered method is covered,
/// and doubles as a heartbeat that proves the metrics pipeline end-to-end
/// without needing an LLM call. Under the noop meter (endpoint unset) the
/// record is a no-op.
pub(crate) fn record_rpc_call(method: &'static str) {
    static COUNTER: OnceLock<opentelemetry::metrics::Counter<u64>> = OnceLock::new();
    let counter = COUNTER.get_or_init(|| {
        opentelemetry::global::meter("smedjad")
            .u64_counter("smedja_rpc_requests_total")
            .with_description("JSON-RPC requests served, by method")
            .build()
    });
    counter.add(1, &[KeyValue::new("rpc.method", method)]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otlp_endpoint_treats_empty_as_unset() {
        // SAFETY: single-threaded test; env var is cleaned up immediately after.
        std::env::set_var(ENV_OTLP_ENDPOINT, "");
        assert_eq!(otlp_endpoint(), None);
        std::env::set_var(ENV_OTLP_ENDPOINT, "http://localhost:4318");
        assert_eq!(otlp_endpoint().as_deref(), Some("http://localhost:4318"));
        std::env::remove_var(ENV_OTLP_ENDPOINT);
    }

    #[test]
    fn signal_endpoint_appends_path_and_tolerates_trailing_slash() {
        assert_eq!(
            signal_endpoint("http://localhost:4318", "/v1/traces"),
            "http://localhost:4318/v1/traces"
        );
        assert_eq!(
            signal_endpoint("http://localhost:4318/", "/v1/metrics"),
            "http://localhost:4318/v1/metrics"
        );
    }

    #[test]
    fn record_rpc_call_is_safe_under_noop_meter() {
        // No meter provider installed in tests → records into the noop meter.
        // The assertion is simply that this neither panics nor blocks.
        record_rpc_call("ping");
        record_rpc_call("session.create");
    }
}
