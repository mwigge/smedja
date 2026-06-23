//! Audit / cowork RPC handlers: `audit.list`, `cowork.set/approve/deny/modify/pending`.

use std::sync::Arc;

use serde_json::{json, Value};
use smedja_rpc::{codes, RpcError};

use crate::cowork::CoworkGate;
use crate::handlers::HandlerState;
use crate::{ingot_err, missing_param};

/// Handles `audit.list`.
///
/// # Errors
///
/// Returns an error when `session_id` is missing or the ingot query fails.
pub(crate) async fn list(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let events = ig
        .list_audit_events(&session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    let events_json: Vec<Value> = events
        .into_iter()
        .map(|ev| serde_json::to_value(&ev).unwrap_or(Value::Null))
        .collect();
    Ok(json!({ "events": events_json }))
}

/// Handles `cowork.set`: toggles cowork mode and manages the per-session gate.
///
/// # Errors
///
/// Returns an error when `session_id` or `enabled` is missing, or the ingot
/// write fails.
pub(crate) async fn set(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let gates = state.gates;
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let enabled = params
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| missing_param("enabled"))?;
    ig.update_session_cowork_mode(&session_id, enabled)
        .await
        .map_err(|e| ingot_err(&e))?;

    // Manage the per-session gate.
    let mut g = gates.lock().await;
    if enabled {
        g.entry(session_id.clone())
            .or_insert_with(|| Arc::new(CoworkGate::default()));
    } else {
        g.remove(&session_id);
    }

    Ok(json!({ "session_id": session_id, "cowork_mode": enabled }))
}

/// Looks up the cowork gate for `session_id`, erroring when none is registered.
async fn gate_for(state: &HandlerState, session_id: &str) -> Result<Arc<CoworkGate>, RpcError> {
    state
        .gates
        .lock()
        .await
        .get(session_id)
        .cloned()
        .ok_or_else(|| {
            RpcError::new(
                codes::INTERNAL_ERROR,
                format!("no cowork gate for session: {session_id}"),
            )
        })
}

/// Handles `cowork.approve`.
///
/// # Errors
///
/// Returns an error when `session_id`/`id` is missing or no gate is registered.
pub(crate) async fn approve(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let id = params["id"]
        .as_str()
        .ok_or_else(|| missing_param("id"))?
        .to_owned();
    let gate = gate_for(&state, &session_id).await?;
    let found = gate.approve(&id).await;
    Ok(json!({ "id": id, "resolved": found }))
}

/// Handles `cowork.deny`.
///
/// # Errors
///
/// Returns an error when `session_id`/`id` is missing or no gate is registered.
pub(crate) async fn deny(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let id = params["id"]
        .as_str()
        .ok_or_else(|| missing_param("id"))?
        .to_owned();
    let reason = params["reason"].as_str().unwrap_or("denied").to_owned();
    let gate = gate_for(&state, &session_id).await?;
    let found = gate.deny(&id, reason).await;
    Ok(json!({ "id": id, "resolved": found }))
}

/// Handles `cowork.modify`.
///
/// # Errors
///
/// Returns an error when `session_id`/`id` is missing or no gate is registered.
pub(crate) async fn modify(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let id = params["id"]
        .as_str()
        .ok_or_else(|| missing_param("id"))?
        .to_owned();
    let instruction = params["instruction"].as_str().unwrap_or("").to_owned();
    let gate = gate_for(&state, &session_id).await?;
    let found = gate.modify(&id, instruction).await;
    Ok(json!({ "id": id, "resolved": found }))
}

/// Handles `cowork.pending`.
///
/// # Errors
///
/// Returns an error when `session_id` is missing or no gate is registered.
pub(crate) async fn pending(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let gate = gate_for(&state, &session_id).await?;
    let pending = gate.list_pending().await;
    let out: Vec<Value> = pending
        .into_iter()
        .map(|(id, p)| {
            json!({
                "id": id,
                "tool": p.tool,
                "step_n": p.step_n,
                "args": p.args_scrubbed,
                "reasoning": p.reasoning,
            })
        })
        .collect();
    Ok(Value::Array(out))
}
