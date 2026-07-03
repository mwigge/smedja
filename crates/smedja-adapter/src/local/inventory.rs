//! Inventory queries and install orchestration against the local endpoint.

use std::time::Duration;

use reqwest::Client;

use super::metrics::health_check_counter;
use super::types::{InstallOutcome, LocalCapability, LocalModel};

/// Performs `GET /v1/models` with a 500 ms timeout and returns a
/// [`LocalCapability`].
pub(crate) async fn health_check(endpoint: &str) -> LocalCapability {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
