//! The [`LocalProvider`] and the swap-request primitive behind it.

use std::time::Duration;

use reqwest::Client;

use super::inventory::health_check;
use super::metrics::record_local_swap;
use super::types::{LocalCapability, LocalModel, SwapOutcome};
use crate::{CallOptions, DeltaStream, Message, OpenAiProvider, Provider};

/// Provider that delegates to a locally-running rs-llmctl instance.
///
/// Wraps [`OpenAiProvider`] pointing at `SMEDJA_LOCAL_ENDPOINT` with an empty
/// API key.  Call [`LocalProvider::connect`] to perform the health check before
/// using the provider in a turn.
pub struct LocalProvider {
    inner: OpenAiProvider,
    /// Base endpoint serving the OpenAI-compatible API (`SMEDJA_LOCAL_ENDPOINT`).
    endpoint: String,
    /// Swap-proxy endpoint (`SMEDJA_LOCAL_SWAP_ENDPOINT`, defaulting to `endpoint`).
    swap_endpoint: String,
    /// Capability snapshot populated by [`LocalProvider::connect`].
    pub capability: LocalCapability,
}

impl LocalProvider {
    /// Performs a health check against the local endpoint and returns a
    /// [`LocalProvider`].
    ///
    /// Reads `SMEDJA_LOCAL_ENDPOINT` (default `http://127.0.0.1:9090`) and
    /// `SMEDJA_LOCAL_SWAP_ENDPOINT` (default: the same base endpoint).
    /// Sets `capability.healthy = false` if `GET /v1/models` fails or times
    /// out (500 ms deadline).
    ///
    /// Emits an `OTel` span `smedja.local.health_check` with attributes
    /// `smedja.local.endpoint` and `smedja.local.model_id`.
    pub async fn connect() -> Self {
        let endpoint = std::env::var("SMEDJA_LOCAL_ENDPOINT")
            .unwrap_or_else(|_| "http://127.0.0.1:9090".to_owned());
        let swap_endpoint =
            std::env::var("SMEDJA_LOCAL_SWAP_ENDPOINT").unwrap_or_else(|_| endpoint.clone());

        let capability = health_check(&endpoint).await;

        Self {
            inner: OpenAiProvider::new(endpoint.clone(), ""),
            endpoint,
            swap_endpoint,
            capability,
        }
    }

    /// Returns the full local-model inventory captured by the last health check.
    #[must_use]
    pub fn models(&self) -> &[LocalModel] {
        &self.capability.inventory
    }

    /// Hot-swaps the active local model through the swap proxy.
    ///
    /// Issues a swap request to `SMEDJA_LOCAL_SWAP_ENDPOINT` (`POST
    /// {swap_endpoint}/swap`). On success the proxy serves `model_id` for
    /// subsequent `stream_chat` calls and the returned [`SwapOutcome`] reports
    /// `explicit_swap = true`.
    ///
    /// When the explicit swap endpoint is unsupported (any non-success status),
    /// falls back to setting the active-model label only — a model-routing proxy
    /// then honours the chosen `model` on subsequent requests — and reports
    /// `explicit_swap = false`. The in-memory `active_model_id` is updated on
    /// either path.
    ///
    /// Emits an `OTel` span `smedja.local.swap` (`smedja.local.from_model`,
    /// `smedja.local.to_model`, swap latency) and increments
    /// `smedja_local_swaps_total`.
    ///
    /// # Errors
    ///
    /// Returns [`crate::AdapterError`] only when the swap endpoint is wholly
    /// unreachable (transport error); an unsupported endpoint degrades to the
    /// label-only fallback rather than erroring.
    #[must_use = "the swap outcome reports whether the explicit endpoint was honoured"]
    pub async fn swap_model(&mut self, model_id: &str) -> Result<SwapOutcome, crate::AdapterError> {
        let from = self.capability.active_model_id.clone().unwrap_or_default();
        let span = tracing::info_span!(
            "smedja.local.swap",
            "smedja.local.from_model" = %from,
            "smedja.local.to_model" = %model_id,
        );
        let _enter = span.enter();

        let started = std::time::Instant::now();
        let outcome = self
            .issue_swap(model_id)
            .await
            .inspect_err(|_| record_local_swap(false))?;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        self.capability.active_model_id = Some(model_id.to_owned());
        tracing::info!(
            "smedja.local.swap_latency_ms" = latency_ms,
            explicit_swap = outcome.explicit_swap,
            "otel.status_code" = "OK",
            "local model swap applied",
        );
        record_local_swap(true);
        Ok(outcome)
    }

    /// Issues the swap request to the proxy, returning the outcome.
    async fn issue_swap(&self, model_id: &str) -> Result<SwapOutcome, crate::AdapterError> {
        issue_swap_request(&self.swap_endpoint, model_id).await
    }

    /// Returns the OpenAI-compatible base endpoint this provider targets.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the swap-proxy endpoint this provider issues swap requests to.
    #[must_use]
    pub fn swap_endpoint(&self) -> &str {
        &self.swap_endpoint
    }
}

/// Issues a swap request to a llama-swap-compatible proxy at `swap_endpoint`.
///
/// `POST {swap_endpoint}/swap` with body `{ "model": model_id }`. A success
/// status uses the explicit-swap path (`explicit_swap = true`); any non-success
/// status degrades to the label-only fallback (`explicit_swap = false`) so a
/// model-routing proxy honours the chosen `model` on subsequent requests.
///
/// This is the reusable primitive behind [`LocalProvider::swap_model`]; the
/// daemon's `local.swap` handler calls it directly so the active-model label can
/// move without rebuilding the pool's `stream_chat` provider.
///
/// # Errors
///
/// Returns [`crate::AdapterError::Request`] only when the proxy is wholly
/// unreachable (transport error).
pub async fn issue_swap_request(
    swap_endpoint: &str,
    model_id: &str,
) -> Result<SwapOutcome, crate::AdapterError> {
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| crate::AdapterError::Request(e.to_string()))?;
    let url = format!("{}/swap", swap_endpoint.trim_end_matches('/'));
    match client
        .post(&url)
        .json(&serde_json::json!({ "model": model_id }))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => Ok(SwapOutcome {
            active_model_id: model_id.to_owned(),
            explicit_swap: true,
        }),
        Ok(_) => Ok(SwapOutcome {
            active_model_id: model_id.to_owned(),
            explicit_swap: false,
        }),
        Err(e) => Err(crate::AdapterError::Request(e.to_string())),
    }
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
    // Holds the env lock across the connect await on purpose: the env mutation
    // must stay serialized for the whole call so a sibling test cannot change it.
    #[allow(clippy::await_holding_lock)]
    async fn connect_reports_unhealthy_when_no_server() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        // Port 19999 is chosen to be very unlikely to have anything listening.
        std::env::set_var("SMEDJA_LOCAL_ENDPOINT", "http://127.0.0.1:19999");
        let provider = LocalProvider::connect().await;
        assert!(
            !provider.capability.healthy,
            "expected healthy=false when no server is listening"
        );
        assert!(
            provider.capability.inventory.is_empty(),
            "expected empty inventory when unhealthy"
        );
        assert!(
            provider.capability.active_model_id.is_none(),
            "expected no active model when unhealthy"
        );
    }

    /// The endpoint default must fall back to `http://127.0.0.1:9090` when
    /// `SMEDJA_LOCAL_ENDPOINT` is not set.
    #[tokio::test]
    // Holds the env lock across the connect await on purpose: the env mutation
    // must stay serialized for the whole call so a sibling test cannot change it.
    #[allow(clippy::await_holding_lock)]
    async fn connect_uses_default_endpoint_when_env_not_set() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        // Remove the env var so the default kicks in.
        std::env::remove_var("SMEDJA_LOCAL_ENDPOINT");
        // We don't assert the result of the health check (port 9090 may or may
        // not be running in CI); we only assert the call completes without
        // panicking and within the timeout.
        let _provider = LocalProvider::connect().await;
    }

    /// A successful explicit swap reports the new active model and `explicit_swap`.
    #[tokio::test]
    async fn swap_model_uses_explicit_endpoint_on_success() {
        let server = MockSwapServer::spawn(200).await;
        let mut provider = LocalProvider {
            inner: OpenAiProvider::new(server.base_url(), ""),
            endpoint: server.base_url(),
            swap_endpoint: server.base_url(),
            capability: LocalCapability {
                inventory: vec![LocalModel {
                    id: "qwen3-14b".to_owned(),
                    est_vram_mb: None,
                }],
                active_model_id: Some("qwen3-14b".to_owned()),
                healthy: true,
            },
        };
        let outcome = provider.swap_model("llama3-8b").await.expect("swap ok");
        assert!(
            outcome.explicit_swap,
            "200 must take the explicit-swap path"
        );
        assert_eq!(outcome.active_model_id, "llama3-8b");
        assert_eq!(
            provider.capability.active_model_id.as_deref(),
            Some("llama3-8b"),
            "active model must update in place"
        );
    }

    /// When the swap endpoint is unsupported (non-success), the fallback sets the
    /// active-model label only and reports `explicit_swap = false`.
    #[tokio::test]
    async fn swap_model_falls_back_to_label_on_unsupported_endpoint() {
        let server = MockSwapServer::spawn(404).await;
        let mut provider = LocalProvider {
            inner: OpenAiProvider::new(server.base_url(), ""),
            endpoint: server.base_url(),
            swap_endpoint: server.base_url(),
            capability: LocalCapability {
                inventory: vec![LocalModel {
                    id: "qwen3-14b".to_owned(),
                    est_vram_mb: None,
                }],
                active_model_id: Some("qwen3-14b".to_owned()),
                healthy: true,
            },
        };
        let outcome = provider.swap_model("llama3-8b").await.expect("swap ok");
        assert!(
            !outcome.explicit_swap,
            "404 must take the label-only fallback"
        );
        assert_eq!(outcome.active_model_id, "llama3-8b");
        assert_eq!(
            provider.capability.active_model_id.as_deref(),
            Some("llama3-8b"),
            "active model label must still update on fallback"
        );
    }

    /// Minimal one-shot HTTP server returning a fixed status for `/swap`.
    struct MockSwapServer {
        port: u16,
    }

    impl MockSwapServer {
        async fn spawn(status: u16) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind ephemeral port");
            let port = listener.local_addr().expect("local addr").port();
            tokio::spawn(async move {
                if let Ok((mut stream, _)) = listener.accept().await {
                    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf).await;
                    let reason = if status == 200 { "OK" } else { "Not Found" };
                    let body = "{}";
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.flush().await;
                }
            });
            Self { port }
        }

        fn base_url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }
    }
}
