//! `local.*` RPC handlers — the control plane for the `local` runner.
//!
//! These handlers orchestrate the external local-serving tools: they read the
//! model inventory and GPU snapshot captured at startup, issue hot-swap requests
//! to the swap proxy, and drive the installer. When no healthy local endpoint
//! was detected at startup the handlers return a structured "local tooling
//! unavailable" error so the daemon stays up and other runners are unaffected.

use std::time::Instant;

use serde_json::{json, Value};
use smedja_adapter::{fit_for, GpuSnapshot, LocalModel};
use smedja_rpc::{codes, RpcError};

use crate::handlers::HandlerState;

/// Returns the structured "local tooling unavailable" error with an install hint.
fn unavailable() -> RpcError {
    RpcError::new(
        codes::INTERNAL_ERROR,
        "local tooling unavailable — no healthy local endpoint was detected at startup; \
         install rs-llmctl and a llama-swap-compatible proxy, then restart smdjad",
    )
}

/// Serialises a model with its advisory fit against the GPU snapshot.
fn model_json(model: &LocalModel, gpu: &GpuSnapshot, active: Option<&str>) -> Value {
    json!({
        "id": model.id,
        "est_vram_mb": model.est_vram_mb,
        "fit": fit_for(model, gpu).label(),
        "active": active == Some(model.id.as_str()),
    })
}

/// Serialises a GPU snapshot, including the explicit no-GPU shape.
fn gpu_json(gpu: &GpuSnapshot) -> Value {
    json!({
        "device": gpu.device,
        "vram_total_mb": gpu.vram_total_mb,
        "vram_free_mb": gpu.vram_free_mb,
        "detected": !gpu.is_none(),
    })
}

/// Handles `local.models`: the GPU-annotated local-model inventory.
///
/// Each entry carries `id`, `est_vram_mb`, an advisory `fit`
/// (`fits|tight|exceeds|unknown`), and whether it is the active model.
///
/// # Errors
///
/// Returns the "local tooling unavailable" error when no healthy local endpoint
/// was detected at startup.
#[allow(clippy::unused_async)] // uniform handler signature: all handlers are async fns
pub(crate) async fn models(state: HandlerState, _params: Value) -> Result<Value, RpcError> {
    let local = state
        .provider_pool
        .local_control()
        .ok_or_else(unavailable)?;
    let active = local.active_model_id();
    let models: Vec<Value> = local
        .inventory
        .iter()
        .map(|m| model_json(m, &local.gpu, active.as_deref()))
        .collect();
    Ok(json!({
        "active_model_id": active,
        "models": models,
        "gpu": gpu_json(&local.gpu),
    }))
}

/// Handles `local.gpu`: the cached GPU snapshot (or the explicit no-GPU shape).
///
/// # Errors
///
/// Returns the "local tooling unavailable" error when no healthy local endpoint
/// was detected at startup.
#[allow(clippy::unused_async)] // uniform handler signature: all handlers are async fns
pub(crate) async fn gpu(state: HandlerState, _params: Value) -> Result<Value, RpcError> {
    let local = state
        .provider_pool
        .local_control()
        .ok_or_else(unavailable)?;
    Ok(gpu_json(&local.gpu))
}

/// Handles `local.swap { model }`: hot-swaps the active local model in place.
///
/// Issues the swap request to the swap proxy and, on success, updates the pool
/// entry's `active_model_id` under its lock — no provider is rebuilt. Reports the
/// swap round-trip latency and whether the explicit swap endpoint was honoured.
///
/// # Errors
///
/// Returns the "local tooling unavailable" error when no healthy local endpoint
/// exists, `INVALID_PARAMS` when `model` is missing, and `INTERNAL_ERROR` when
/// the swap proxy is unreachable.
pub(crate) async fn swap(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let local = state
        .provider_pool
        .local_control()
        .ok_or_else(unavailable)?;
    let model = params
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new(codes::INVALID_PARAMS, "missing parameter: model"))?;

    let from = local.active_model_id();
    let span = tracing::info_span!(
        "smedja.local.swap",
        "smedja.local.from_model" = from.as_deref().unwrap_or(""),
        "smedja.local.to_model" = model,
    );
    let _enter = span.enter();

    let started = Instant::now();
    let outcome = smedja_adapter::issue_swap_request(&local.swap_endpoint, model)
        .await
        .inspect_err(|_| smedja_adapter::record_local_swap(false))
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("local swap failed: {e}")))?;
    let swap_latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    local.set_active_model_id(&outcome.active_model_id);
    smedja_adapter::record_local_swap(true);
    tracing::info!(
        "smedja.local.swap_latency_ms" = swap_latency_ms,
        explicit_swap = outcome.explicit_swap,
        "otel.status_code" = "OK",
        "local model swap applied",
    );

    Ok(json!({
        "from_model": from,
        "active_model_id": outcome.active_model_id,
        "explicit_swap": outcome.explicit_swap,
        "swap_latency_ms": swap_latency_ms,
    }))
}

/// Handles `local.install { model }`: drives the external installer and verifies.
///
/// Reports `installed = true` only when the installer exits zero **and** the
/// model afterwards appears in `/v1/models`.
///
/// # Errors
///
/// Returns the "local tooling unavailable" error when no healthy local endpoint
/// exists or the installer binary cannot be spawned, and `INVALID_PARAMS` when
/// `model` is missing.
pub(crate) async fn install(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let endpoint = {
        let local = state
            .provider_pool
            .local_control()
            .ok_or_else(unavailable)?;
        local.endpoint.clone()
    };
    let model = params
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new(codes::INVALID_PARAMS, "missing parameter: model"))?;

    let outcome = smedja_adapter::install_model(&endpoint, model)
        .await
        .map_err(|_| unavailable())?;

    Ok(json!({
        "model": model,
        "installed": outcome.installed,
        "installer_ok": outcome.installer_ok,
        "present_in_inventory": outcome.present_in_inventory,
        "log_tail": outcome.log_tail,
    }))
}

/// Test-only helpers that operate directly on a [`LocalControl`], exercising the
/// response shapes without a full daemon `HandlerState`.
#[cfg(test)]
pub(crate) mod logic {
    use super::{gpu_json, model_json};
    use serde_json::{json, Value};
    use smedja_adapter::GpuSnapshot;

    use crate::provider_pool::LocalControl;

    /// The `local.models` response body for a given control plane.
    pub(crate) fn models_body(local: &LocalControl) -> Value {
        let active = local.active_model_id();
        let models: Vec<Value> = local
            .inventory
            .iter()
            .map(|m| model_json(m, &local.gpu, active.as_deref()))
            .collect();
        json!({
            "active_model_id": active,
            "models": models,
            "gpu": gpu_json(&local.gpu),
        })
    }

    /// The `local.gpu` response body for a given snapshot.
    pub(crate) fn gpu_body(gpu: &GpuSnapshot) -> Value {
        gpu_json(gpu)
    }
}

#[cfg(test)]
mod tests {
    use super::logic;

    use smedja_adapter::{GpuSnapshot, LocalModel};

    use crate::provider_pool::LocalControl;

    fn control_with_gpu(gpu: GpuSnapshot, active: Option<&str>) -> LocalControl {
        LocalControl::new(
            "http://127.0.0.1:9090".to_owned(),
            "http://127.0.0.1:9090".to_owned(),
            vec![
                LocalModel {
                    id: "qwen3-14b".to_owned(),
                    est_vram_mb: Some(9000),
                },
                LocalModel {
                    id: "huge-70b".to_owned(),
                    est_vram_mb: Some(48000),
                },
                LocalModel {
                    id: "no-meta".to_owned(),
                    est_vram_mb: None,
                },
            ],
            gpu,
            active.map(ToOwned::to_owned),
        )
    }

    /// `local.models` returns every model annotated with its GPU fit.
    #[test]
    fn models_returns_gpu_annotated_inventory() {
        let gpu = GpuSnapshot {
            device: Some("RTX 4090".to_owned()),
            vram_total_mb: Some(24000),
            vram_free_mb: Some(20000),
        };
        let local = control_with_gpu(gpu, Some("qwen3-14b"));
        let body = logic::models_body(&local);

        let models = body["models"].as_array().expect("models array");
        assert_eq!(models.len(), 3, "every inventory entry must be listed");
        assert_eq!(models[0]["id"], "qwen3-14b");
        assert_eq!(models[0]["fit"], "fits", "9000 MiB fits 20000 MiB free");
        assert_eq!(models[0]["active"], true);
        assert_eq!(models[1]["fit"], "exceeds", "48000 MiB exceeds 20000 free");
        assert_eq!(
            models[2]["fit"], "unknown",
            "no est_vram_mb must annotate unknown"
        );
        assert_eq!(body["active_model_id"], "qwen3-14b");
    }

    /// `local.gpu` reports a detected snapshot.
    #[test]
    fn gpu_returns_detected_snapshot() {
        let gpu = GpuSnapshot {
            device: Some("RTX 4090".to_owned()),
            vram_total_mb: Some(24000),
            vram_free_mb: Some(20000),
        };
        let body = logic::gpu_body(&gpu);
        assert_eq!(body["device"], "RTX 4090");
        assert_eq!(body["vram_total_mb"], 24000);
        assert_eq!(body["detected"], true);
    }

    /// `local.gpu` reports the explicit no-GPU shape (every field null, not error).
    #[test]
    fn gpu_returns_explicit_no_gpu_shape() {
        let body = logic::gpu_body(&GpuSnapshot::none());
        assert!(body["device"].is_null());
        assert!(body["vram_total_mb"].is_null());
        assert!(body["vram_free_mb"].is_null());
        assert_eq!(body["detected"], false);
    }

    /// A swap updates the active model in place without rebuilding the provider.
    #[test]
    fn swap_updates_active_model_in_place() {
        let local = control_with_gpu(GpuSnapshot::none(), Some("qwen3-14b"));
        let previous = local.set_active_model_id("huge-70b");
        assert_eq!(previous.as_deref(), Some("qwen3-14b"));
        assert_eq!(
            local.active_model_id().as_deref(),
            Some("huge-70b"),
            "swap must mutate active_model_id in place"
        );
    }
}
