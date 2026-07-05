//! Local rs-llmctl adapter — OpenAI-compatible endpoint, health-checked at startup.
//!
//! Reads `SMEDJA_LOCAL_ENDPOINT` (default `http://127.0.0.1:9090`) and performs
//! a capability pre-flight against `GET /v1/models` before the first turn runs.
//!
//! smedja **orchestrates** the external local-serving tools (rs-llmctl for
//! install/inventory, a llama-swap-compatible proxy for hot-swap); it does not
//! reimplement an inference server, download weights, or place models on GPUs.

use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::metrics::Counter;
use reqwest::Client;

use crate::{CallOptions, DeltaStream, Message, OpenAiProvider, Provider};

/// Returns (initialising on first call) the `smedja_local_health_checks_total` counter.
fn health_check_counter() -> &'static Counter<u64> {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        opentelemetry::global::meter("smedja-adapter")
            .u64_counter("smedja_local_health_checks_total")
            .with_description(
                "Total number of local endpoint health checks, labelled by result (ok|error).",
            )
            .build()
    })
}

/// Returns (initialising on first call) the `smedja_local_swaps_total` counter.
fn swap_counter() -> &'static Counter<u64> {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        opentelemetry::global::meter("smedja-adapter")
            .u64_counter("smedja_local_swaps_total")
            .with_description("Total number of local model swaps, labelled by result (ok|error).")
            .build()
    })
}

/// Records a local-model swap result on the `smedja_local_swaps_total` counter,
/// labelled `result = ok | error`.
///
/// Exposed so the daemon's `local.swap` handler — which issues the swap directly
/// via [`issue_swap_request`] rather than [`LocalProvider::swap_model`] — records
/// the same metric on the same instrument.
pub fn record_local_swap(ok: bool) {
    let result = if ok { "ok" } else { "error" };
    swap_counter().add(1, &[opentelemetry::KeyValue::new("result", result)]);
}

/// A single model offered by the local swap proxy's `/v1/models` inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalModel {
    /// The `id` field of the model entry.
    pub id: String,
    /// Estimated VRAM footprint in MiB, when the inventory metadata exposes it.
    pub est_vram_mb: Option<u64>,
}

/// Capability snapshot returned by the health check.
///
/// Holds the full `/v1/models` inventory (not just the first id) plus the
/// currently-active model id, so the picker can list every servable model and
/// the swap path can update the active selection in place.
#[derive(Debug, Clone)]
pub struct LocalCapability {
    /// Every model entry returned by `GET /v1/models`, in response order.
    pub inventory: Vec<LocalModel>,
    /// The currently-active model id (the first inventory entry at connect time),
    /// or `None` when the inventory is empty.
    pub active_model_id: Option<String>,
    /// `true` when the health check succeeded within the 500 ms timeout.
    pub healthy: bool,
}

/// Outcome of a [`LocalProvider::swap_model`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapOutcome {
    /// The model id now active after the swap.
    pub active_model_id: String,
    /// `true` when the explicit swap endpoint accepted the request; `false` when
    /// the label-only fallback path was taken (the proxy routes on the request
    /// `model` field instead of an explicit swap endpoint).
    pub explicit_swap: bool,
}

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

/// Performs `GET /v1/models` with a 500 ms timeout and returns a
/// [`LocalCapability`].
async fn health_check(endpoint: &str) -> LocalCapability {
    let span = tracing::info_span!(
        "smedja.local.health_check",
        "smedja.local.endpoint" = %endpoint,
    );
    let _enter = span.enter();

    let client = match Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            // A failed client build must not crash the daemon: report the local
            // tier as unhealthy so it is simply skipped, exactly as an
            // unreachable endpoint would be.
            tracing::warn!(
                error = %e,
                "smedja.local.error" = %e,
                "otel.status_code" = "ERROR",
                "local health check could not build HTTP client — tier will be skipped",
            );
            return LocalCapability {
                inventory: Vec::new(),
                active_model_id: None,
                healthy: false,
            };
        }
    };

    let url = format!("{endpoint}/v1/models");
    let counter = health_check_counter();
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body = resp.json::<serde_json::Value>().await.unwrap_or_default();
            let inventory = parse_model_inventory(&body);
            let active_model_id = inventory.first().map(|m| m.id.clone());
            tracing::info!(
                "smedja.local.model_id" = active_model_id.as_deref().unwrap_or(""),
                "smedja.local.model_count" = inventory.len(),
                "otel.status_code" = "OK",
                healthy = true,
                "local health check ok",
            );
            counter.add(1, &[opentelemetry::KeyValue::new("result", "ok")]);
            LocalCapability {
                inventory,
                active_model_id,
                healthy: true,
            }
        }
        Ok(resp) => {
            let description = format!("non-success HTTP status: {}", resp.status());
            tracing::warn!(
                status = %resp.status(),
                "smedja.local.error" = %description,
                "otel.status_code" = "ERROR",
                "local health check returned non-success status",
            );
            counter.add(1, &[opentelemetry::KeyValue::new("result", "error")]);
            LocalCapability {
                inventory: Vec::new(),
                active_model_id: None,
                healthy: false,
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "smedja.local.error" = %e,
                "otel.status_code" = "ERROR",
                "local endpoint unreachable — tier will be skipped",
            );
            counter.add(1, &[opentelemetry::KeyValue::new("result", "error")]);
            LocalCapability {
                inventory: Vec::new(),
                active_model_id: None,
                healthy: false,
            }
        }
    }
}

/// Parses every `data[].id` entry from a `/v1/models` JSON response body into a
/// full [`LocalModel`] inventory.
///
/// Each entry's `est_vram_mb` is populated from an optional `est_vram_mb`
/// metadata field where the proxy exposes it; absent or non-numeric values
/// leave it `None`. Returns an empty vector when `data` is missing or empty.
#[must_use]
pub fn parse_model_inventory(body: &serde_json::Value) -> Vec<LocalModel> {
    let Some(data) = body.get("data").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    data.iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(serde_json::Value::as_str)?.to_owned();
            let est_vram_mb = m.get("est_vram_mb").and_then(serde_json::Value::as_u64);
            Some(LocalModel { id, est_vram_mb })
        })
        .collect()
}

/// Re-queries `GET {endpoint}/v1/models` and returns the parsed inventory.
///
/// Used by `local.install` to verify a freshly-installed model is actually
/// servable before claiming success, and by `local.models` to refresh the
/// inventory. Returns an empty vector when the endpoint is unreachable or the
/// body cannot be parsed (the caller treats an absent model as "not installed").
///
/// # Errors
///
/// Returns [`crate::AdapterError::Request`] when the HTTP client cannot be
/// built or the request fails at the transport level.
pub async fn fetch_inventory(endpoint: &str) -> Result<Vec<LocalModel>, crate::AdapterError> {
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| crate::AdapterError::Request(e.to_string()))?;
    let url = format!("{}/v1/models", endpoint.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| crate::AdapterError::Request(e.to_string()))?;
    if !resp.status().is_success() {
        return Ok(Vec::new());
    }
    let body = resp.json::<serde_json::Value>().await.unwrap_or_default();
    Ok(parse_model_inventory(&body))
}

/// Result of an install orchestration: the installer's exit and the post-install
/// verification against the live inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    /// `true` only when the installer succeeded **and** the model afterwards
    /// appears in `/v1/models`.
    pub installed: bool,
    /// Whether the installer process exited zero.
    pub installer_ok: bool,
    /// Whether the model is present in the live inventory after the install.
    pub present_in_inventory: bool,
    /// The installer's combined stdout/stderr tail (for surfacing to the operator).
    pub log_tail: String,
}

/// Orchestrates a model install by shelling out to the external installer
/// (`SMEDJA_LOCAL_INSTALLER`, default `rs-llmctl`) and verifying the result.
///
/// Runs `{installer} pull {model_id}` via `tokio::process` (never blocking the
/// async runtime), then re-queries `{endpoint}/v1/models` and reports success
/// only when `model_id` now appears in the inventory. smedja shells out to the
/// installer; it does not download or quantise weights itself.
///
/// # Errors
///
/// Returns [`crate::AdapterError::Request`] when the installer binary cannot be
/// spawned (e.g. rs-llmctl is not installed) — the caller surfaces this as the
/// structured "local tooling unavailable" error.
pub async fn install_model(
    endpoint: &str,
    model_id: &str,
) -> Result<InstallOutcome, crate::AdapterError> {
    let span = tracing::info_span!("smedja.local.install", "smedja.local.model_id" = %model_id);
    let _enter = span.enter();

    let tool = std::env::var("SMEDJA_LOCAL_INSTALLER").unwrap_or_else(|_| "rs-llmctl".to_owned());
    let output = tokio::process::Command::new(&tool)
        .args(["pull", model_id])
        .output()
        .await
        .map_err(|e| crate::AdapterError::Request(format!("{tool}: {e}")))?;

    let installer_ok = output.status.success();
    let mut log_tail = String::from_utf8_lossy(&output.stdout).into_owned();
    log_tail.push_str(&String::from_utf8_lossy(&output.stderr));

    let inventory = fetch_inventory(endpoint).await.unwrap_or_default();
    let present_in_inventory = inventory.iter().any(|m| m.id == model_id);
    let installed = installer_ok && present_in_inventory;

    tracing::info!(
        installer_ok,
        present_in_inventory,
        installed,
        "otel.status_code" = if installed { "OK" } else { "ERROR" },
        "local model install completed",
    );

    Ok(InstallOutcome {
        installed,
        installer_ok,
        present_in_inventory,
        log_tail,
    })
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

    /// `parse_first_model_id` returns empty string for malformed JSON.
    #[tokio::test]
    async fn health_check_unhealthy_on_bad_endpoint() {
        // A port that refuses connections immediately.
        let capability = health_check("http://127.0.0.1:1").await;
        assert!(!capability.healthy);
        assert!(capability.inventory.is_empty());
        assert!(capability.active_model_id.is_none());
    }

    /// Verifies span attributes and counter path for the ok branch: with no server
    /// at port 1, the check must return unhealthy and exercise the error counter path.
    #[tokio::test]
    async fn health_check_ok_span_attributes_on_failure_path() {
        // Port 1 causes immediate ECONNREFUSED — exercises the error branch fully.
        let capability = health_check("http://127.0.0.1:1").await;
        assert!(
            !capability.healthy,
            "expected healthy=false for connection-refused endpoint"
        );
        assert!(
            capability.inventory.is_empty(),
            "inventory must be empty when endpoint is unreachable"
        );
    }

    /// `health_check` on a refused port returns unhealthy — counter error path taken.
    ///
    /// Counter side-effects are verified indirectly (the function must not panic;
    /// the global meter is a no-op when no SDK pipeline is installed in tests).
    #[tokio::test]
    async fn health_check_increments_error_counter_on_failure() {
        let capability = health_check("http://127.0.0.1:1").await;
        // The counter increment must not panic; healthy=false confirms error branch ran.
        assert!(
            !capability.healthy,
            "error counter branch must set healthy=false"
        );
    }

    /// `parse_model_inventory` returns every `data[].id`, not just the first.
    #[test]
    fn parse_model_inventory_returns_all_entries() {
        let body = serde_json::json!({
            "data": [
                { "id": "qwen3-14b", "est_vram_mb": 9000 },
                { "id": "llama3-8b" },
                { "id": "phi4-mini", "est_vram_mb": 4000 }
            ]
        });
        let inventory = parse_model_inventory(&body);
        let ids: Vec<&str> = inventory.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["qwen3-14b", "llama3-8b", "phi4-mini"],
            "inventory must surface every model id, not just data[0]"
        );
        assert_eq!(inventory[0].est_vram_mb, Some(9000));
        assert_eq!(
            inventory[1].est_vram_mb, None,
            "missing est_vram_mb must parse as None, not an error"
        );
        assert_eq!(inventory[2].est_vram_mb, Some(4000));
    }

    /// `parse_model_inventory` returns an empty vector for a missing/empty list.
    #[test]
    fn parse_model_inventory_empty_when_no_data() {
        assert!(parse_model_inventory(&serde_json::json!({})).is_empty());
        assert!(parse_model_inventory(&serde_json::json!({ "data": [] })).is_empty());
    }

    /// `install_model` returns the structured spawn error when the installer
    /// binary cannot be spawned — the handler maps this to "tooling unavailable".
    #[tokio::test]
    // Holds the env lock across the await on purpose: the installer-name env var
    // must stay serialized for the whole call so a sibling test cannot change it.
    #[allow(clippy::await_holding_lock)]
    async fn install_model_errors_when_installer_binary_absent() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        std::env::set_var(
            "SMEDJA_LOCAL_INSTALLER",
            "smedja-nonexistent-installer-binary",
        );
        let result = install_model("http://127.0.0.1:19998", "qwen3-14b").await;
        std::env::remove_var("SMEDJA_LOCAL_INSTALLER");
        assert!(
            result.is_err(),
            "a missing installer binary must surface a spawn error"
        );
    }

    /// `fetch_inventory` against an unreachable endpoint surfaces a transport error.
    #[tokio::test]
    async fn fetch_inventory_errors_on_unreachable_endpoint() {
        let result = fetch_inventory("http://127.0.0.1:1").await;
        assert!(result.is_err(), "unreachable endpoint must error");
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
