//! SRE observability tools: `otel_query`, `metric_query`, and `log_tail`.
//!
//! All three share one pooled [`reqwest::Client`] and short-circuit with a config
//! hint when the relevant `SMEDJA_*` endpoint is unset.

use serde_json::Value;

/// Shared HTTP client for the SRE tools (`otel_query`, `metric_query`,
/// `log_tail`). Built once: connection pooling is lost when a client is rebuilt
/// per call, and the previous per-call `.unwrap_or_default()` silently yielded a
/// timeout-less client on the rare build failure.
fn sre_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default()
    })
}

/// Queries OTel traces for `service` over the requested minute window.
pub(crate) async fn otel_query(input: &Value) -> String {
    let service = input
        .get("service")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let filter = input.get("filter").and_then(|v| v.as_str());
    let range = input
        .get("range_minutes")
        .and_then(Value::as_i64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(60);
    if let Ok(cfg) = smedja_sre::SreConfig::from_env() {
        match smedja_sre::otel_query(sre_http_client(), &cfg, service, filter, range).await {
            Ok(v) => serde_json::to_string(&v).unwrap_or_default(),
            Err(e) => format!("error: {e}"),
        }
    } else {
        "SRE config not available (set SMEDJA_OTLP_ENDPOINT)".into()
    }
}

/// Runs a PromQL query over the requested minute window.
pub(crate) async fn metric_query(input: &Value) -> String {
    let promql = input.get("promql").and_then(|v| v.as_str()).unwrap_or("");
    let range = input
        .get("range_minutes")
        .and_then(Value::as_i64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(60);
    if let Ok(cfg) = smedja_sre::SreConfig::from_env() {
        match smedja_sre::metric_query(sre_http_client(), &cfg, promql, range).await {
            Ok(v) => serde_json::to_string(&v).unwrap_or_default(),
            Err(e) => format!("error: {e}"),
        }
    } else {
        "SRE config not available (set SMEDJA_PROMETHEUS_ENDPOINT)".into()
    }
}

/// Tails the last `lines` log entries for `service`, optionally filtered.
pub(crate) async fn log_tail(input: &Value) -> String {
    let service = input
        .get("service")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let filter = input.get("filter").and_then(|v| v.as_str()).unwrap_or("");
    let lines = input
        .get("lines")
        .and_then(Value::as_i64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(100);
    if let Ok(cfg) = smedja_sre::SreConfig::from_env() {
        match smedja_sre::log_tail(sre_http_client(), &cfg, service, filter, lines).await {
            Ok(v) => serde_json::to_string(&v).unwrap_or_default(),
            Err(e) => format!("error: {e}"),
        }
    } else {
        "SRE config not available (set SMEDJA_LOKI_ENDPOINT)".into()
    }
}
