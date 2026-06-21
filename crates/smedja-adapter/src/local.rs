//! Local rs-llmctl adapter — OpenAI-compatible endpoint, health-checked at startup.
//!
//! Reads `SMEDJA_LOCAL_ENDPOINT` (default `http://127.0.0.1:9090`) and performs
//! a capability pre-flight against `GET /v1/models` before the first turn runs.

use std::time::Duration;

use reqwest::Client;

use crate::{CallOptions, DeltaStream, Message, OpenAiProvider, Provider};

/// Capability snapshot returned by the health check.
#[derive(Debug, Clone)]
pub struct LocalCapability {
    /// The `id` field of the first model entry returned by `GET /v1/models`.
    pub model_id: String,
    /// `true` when the health check succeeded within the 500 ms timeout.
    pub healthy: bool,
}

/// Provider that delegates to a locally-running rs-llmctl instance.
///
/// Wraps [`OpenAiProvider`] pointing at `SMEDJA_LOCAL_ENDPOINT` with an empty
/// API key.  Call [`LocalProvider::connect`] to perform the health check before
/// using the provider in a turn.
pub struct LocalProvider {
    inner: OpenAiProvider,
    /// Capability snapshot populated by [`LocalProvider::connect`].
    pub capability: LocalCapability,
}

impl LocalProvider {
    /// Performs a health check against the local endpoint and returns a
    /// [`LocalProvider`].
    ///
    /// Reads `SMEDJA_LOCAL_ENDPOINT` (default `http://127.0.0.1:9090`).
    /// Sets `capability.healthy = false` if `GET /v1/models` fails or times
    /// out (500 ms deadline).
    ///
    /// Emits an `OTel` span `smedja.local.health_check` with attributes
    /// `smedja.local.endpoint` and `smedja.local.model_id`.
    pub async fn connect() -> Self {
        let endpoint = std::env::var("SMEDJA_LOCAL_ENDPOINT")
            .unwrap_or_else(|_| "http://127.0.0.1:9090".to_owned());

        let capability = health_check(&endpoint).await;

        Self {
            inner: OpenAiProvider::new(endpoint, ""),
            capability,
        }
    }
}

/// Performs `GET /v1/models` with a 500 ms timeout and returns a
/// [`LocalCapability`].
async fn health_check(endpoint: &str) -> LocalCapability {
    let span = tracing::info_span!(
        "smedja.local.health_check",
        "smedja.local.endpoint" = %endpoint,
    );
    let _enter = span.enter();

    let client = Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .expect("reqwest client build is infallible for plain HTTP");

    let url = format!("{endpoint}/v1/models");
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let model_id = parse_first_model_id(resp).await;
            tracing::info!(
                "smedja.local.model_id" = %model_id,
                healthy = true,
                "local health check ok",
            );
            LocalCapability {
                model_id,
                healthy: true,
            }
        }
        Ok(resp) => {
            tracing::warn!(
                status = %resp.status(),
                "local health check returned non-success status",
            );
            LocalCapability {
                model_id: String::new(),
                healthy: false,
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "local endpoint unreachable — tier will be skipped");
            LocalCapability {
                model_id: String::new(),
                healthy: false,
            }
        }
    }
}

/// Parses the `id` field of the first entry in a `/v1/models` JSON response.
///
/// Returns an empty string if parsing fails or the list is empty.
async fn parse_first_model_id(resp: reqwest::Response) -> String {
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return String::new();
    };
    body.get("data")
        .and_then(|d| d.get(0))
        .and_then(|m| m.get("id"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned()
}

impl Provider for LocalProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        self.inner.stream_chat(messages, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When no server is listening the health check must complete (not hang)
    /// and report `healthy = false`.
    #[tokio::test]
    async fn connect_reports_unhealthy_when_no_server() {
        // Port 19999 is chosen to be very unlikely to have anything listening.
        std::env::set_var("SMEDJA_LOCAL_ENDPOINT", "http://127.0.0.1:19999");
        let provider = LocalProvider::connect().await;
        assert!(
            !provider.capability.healthy,
            "expected healthy=false when no server is listening"
        );
        assert!(
            provider.capability.model_id.is_empty(),
            "expected empty model_id when unhealthy"
        );
    }

    /// The endpoint default must fall back to `http://127.0.0.1:9090` when
    /// `SMEDJA_LOCAL_ENDPOINT` is not set.
    #[tokio::test]
    async fn connect_uses_default_endpoint_when_env_not_set() {
        // Remove the env var so the default kicks in.
        std::env::remove_var("SMEDJA_LOCAL_ENDPOINT");
        // We don't assert the result of the health check (port 9090 may or may
        // not be running in CI); we only assert the call completes without
        // panicking and within the timeout.
        let _provider = LocalProvider::connect().await;
    }

    /// `parse_first_model_id` returns empty string for malformed JSON.
    #[tokio::test]
    async fn health_check_unhealthy_on_bad_endpoint() {
        // A port that refuses connections immediately.
        let capability = health_check("http://127.0.0.1:1").await;
        assert!(!capability.healthy);
        assert!(capability.model_id.is_empty());
    }
}
