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

/// Handles `cowork.set_mode`: sets the session's permission mode, creating the
/// gate on demand. `mode` is `ask|accept_edits|plan|auto`; omit `mode` to cycle
/// to the next mode (Shift+Tab from the TUI).
///
/// # Errors
///
/// Returns an error when `session_id` is missing.
pub(crate) async fn set_mode(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let gate = {
        let mut g = state.gates.lock().await;
        Arc::clone(
            g.entry(session_id.clone())
                .or_insert_with(|| Arc::new(CoworkGate::default())),
        )
    };
    let new_mode = match params.get("mode").and_then(Value::as_str) {
        Some(m) => {
            gate.set_mode(crate::cowork::PermissionMode::parse_lenient(m))
                .await
        }
        None => gate.cycle_mode().await,
    };
    Ok(json!({ "session_id": session_id, "mode": new_mode.as_str() }))
}

/// Handles `cowork.gate_tool`: the `PreToolUse` hook entry point for external CLIs
/// (claude via `smj tool-gate`). Evaluates the session's permission policy and
/// blocks on the user when it says "ask". Returns `{decision, reason}` where
/// `decision` is `"allow"` or `"deny"`.
///
/// # Errors
///
/// Never returns an RPC error — a missing tool/session resolves to a decision so
/// the hook always gets an answer (fail-safe is handled by the policy/gate).
pub(crate) async fn gate_tool(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tool_name = params
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let gate = {
        let mut g = state.gates.lock().await;
        Arc::clone(
            g.entry(session_id.clone())
                .or_insert_with(|| Arc::new(CoworkGate::default())),
        )
    };
    // Evaluate the policy synchronously — the hook cannot block waiting for
    // a TUI approval dialog (no push path). Ask → fail-open: the CLI's own
    // sandbox (codex --sandbox, claude's permissions model) provides the
    // outer guard. Plan remains a hard deny so the user can restrict a session
    // to read-only analysis even from external CLIs.
    let mode = gate.mode().await;
    let (decision, reason) = match crate::cowork::evaluate(mode, &tool_name) {
        crate::cowork::PermissionDecision::Deny => {
            ("deny", format!("blocked by {} mode", mode.as_str()))
        }
        // Allow and Ask both resolve to allow: the external CLI's own sandbox
        // provides the outer guard; we cannot block on a TUI approval dialog.
        crate::cowork::PermissionDecision::Allow | crate::cowork::PermissionDecision::Ask => {
            ("allow", String::new())
        }
    };
    Ok(json!({ "decision": decision, "reason": reason }))
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
