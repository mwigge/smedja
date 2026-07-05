//! SRE observability tool bodies: `otel_query`, `metric_query`, `log_tail`.
//!
//! Each builds a short-timeout HTTP client and forwards to the matching
//! `smedja_sre` query. A client-build failure is returned as `Err` (the
//! original arm exited via `return`); every other outcome is `Ok`.

use serde_json::Value;

/// `otel_query` tool body.
pub(crate) async fn otel_query(input: &Value) -> Result<String, String> {
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
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => return Err(format!("error: failed to build HTTP client: {e}")),
        };
        match smedja_sre::otel_query(&client, &cfg, service, filter, range).await {
            Ok(v) => Ok(serde_json::to_string(&v).unwrap_or_default()),
            Err(e) => Ok(format!("error: {e}")),
        }
    } else {
        Ok("SRE config not available (set SMEDJA_OTLP_ENDPOINT)".into())
    }
}

/// `metric_query` tool body.
pub(crate) async fn metric_query(input: &Value) -> Result<String, String> {
    let promql = input.get("promql").and_then(|v| v.as_str()).unwrap_or("");
    let range = input
        .get("range_minutes")
        .and_then(Value::as_i64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(60);
    if let Ok(cfg) = smedja_sre::SreConfig::from_env() {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => return Err(format!("error: failed to build HTTP client: {e}")),
        };
        match smedja_sre::metric_query(&client, &cfg, promql, range).await {
            Ok(v) => Ok(serde_json::to_string(&v).unwrap_or_default()),
            Err(e) => Ok(format!("error: {e}")),
        }
    } else {
        Ok("SRE config not available (set SMEDJA_PROMETHEUS_ENDPOINT)".into())
    }
}

/// `log_tail` tool body.
pub(crate) async fn log_tail(input: &Value) -> Result<String, String> {
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
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => return Err(format!("error: failed to build HTTP client: {e}")),
        };
        match smedja_sre::log_tail(&client, &cfg, service, filter, lines).await {
            Ok(v) => Ok(serde_json::to_string(&v).unwrap_or_default()),
            Err(e) => Ok(format!("error: {e}")),
        }
    } else {
        Ok("SRE config not available (set SMEDJA_LOKI_ENDPOINT)".into())
    }
}
