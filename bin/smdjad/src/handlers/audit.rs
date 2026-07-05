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
/// (claude via `smj tool-gate`). Routes the tool call through the SAME interactive
/// gate the native tool loop uses ([`CoworkGate::gate_tool`]): `Allow`/`Deny`
/// resolve outright per policy, while `Ask` publishes a [`TurnEvent::CoworkRequest`]
/// to the TUI and suspends on the gate until the user answers y/n/m — instead of
/// the old synchronous path that hard-denied every `Ask`.
///
/// Returns `{decision, reason}` (`decision` is `"allow"` or `"deny"`), plus an
/// `updated_input` object when the user chose *modify* with replacement args.
///
/// # Errors
///
/// Never returns an RPC error — a missing tool/session resolves to a decision so
/// the hook always gets an answer (gate timeout/close fails closed to deny).
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
    let tool_input = params.get("tool_input").cloned().unwrap_or(Value::Null);

    let gate = {
        let mut g = state.gates.lock().await;
        Arc::clone(
            g.entry(session_id.clone())
                .or_insert_with(|| Arc::new(CoworkGate::default())),
        )
    };
    // Route through the real interactive gate: read-only/auto/accept-edits allow
    // immediately, plan denies, and `Ask` suspends (publishing a CoworkRequest to
    // the TUI) until the user resolves it — up to the gate's 30-min wait, which
    // the hook's 1800s timeout mirrors. A timeout or channel close fails closed.
    let decision = gate
        .gate_tool(
            0,
            &tool_name,
            tool_input,
            "",
            Some((state.dispatcher.as_ref(), None)),
        )
        .await;
    Ok(gate_response(&decision))
}

/// Maps a resolved cowork [`Decision`] to the `{decision, reason, updated_input}`
/// payload the `smj tool-gate` hook translates into Claude's `PreToolUse` output.
///
/// `Approve` → allow; `Deny` → deny-with-reason. `Modify` carries a free-form
/// instruction; Claude's hook can only rewrite a call via `updatedInput` (a JSON
/// object of replacement args), so a modify instruction is applied as
/// `updated_input` when — and only when — it parses to a JSON object. Otherwise
/// there is no valid rewrite to hand back, so it falls back to deny-with-reason.
/// Kept pure so the mapping is unit-testable.
fn gate_response(decision: &crate::cowork::Decision) -> Value {
    use crate::cowork::Decision;
    match decision {
        Decision::Approve => json!({ "decision": "allow", "reason": "" }),
        Decision::Deny(reason) => json!({ "decision": "deny", "reason": reason }),
        Decision::Modify(instruction) => match serde_json::from_str::<Value>(instruction) {
            Ok(v) if v.is_object() => {
                json!({ "decision": "allow", "reason": "", "updated_input": v })
            }
            _ => json!({
                "decision": "deny",
                "reason": format!(
                    "modify requires replacement arguments as a JSON object; got: {instruction}"
                ),
            }),
        },
    }
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

#[cfg(test)]
mod tests {
    use super::gate_response;
    use crate::cowork::{CoworkGate, Decision, PermissionMode};
    use serde_json::json;
    use smedja_bellows::Dispatcher;
    use std::sync::Arc;

    // ── gate_response mapping (pure) ─────────────────────────────────────────

    #[test]
    fn approve_maps_to_allow() {
        let out = gate_response(&Decision::Approve);
        assert_eq!(out["decision"], "allow");
    }

    #[test]
    fn deny_maps_to_deny_with_reason() {
        let out = gate_response(&Decision::Deny("blocked by plan mode".into()));
        assert_eq!(out["decision"], "deny");
        assert_eq!(out["reason"], "blocked by plan mode");
    }

    #[test]
    fn modify_with_json_object_produces_updated_input() {
        // A modify instruction that parses to a JSON object is handed back as
        // `updated_input` so the claude hook can rewrite the call (allow).
        let out = gate_response(&Decision::Modify(r#"{"command":"ls -a"}"#.into()));
        assert_eq!(out["decision"], "allow");
        assert_eq!(out["updated_input"]["command"], "ls -a");
    }

    #[test]
    fn modify_with_non_object_falls_back_to_deny() {
        // Free-form / non-object modify text can't become valid `updatedInput`,
        // so it must deny rather than silently letting the original call through.
        for instruction in ["use a safer path", "\"just a string\"", "42", "[1,2]"] {
            let out = gate_response(&Decision::Modify(instruction.into()));
            assert_eq!(
                out["decision"], "deny",
                "non-object modify {instruction:?} must deny"
            );
            assert!(out.get("updated_input").is_none());
        }
    }

    // ── interactive gate routing (Ask suspends, not an immediate deny) ────────

    #[tokio::test]
    async fn ask_mode_suspends_then_returns_interactive_decision() {
        // Regression for the fail-closed synchronous gate: under Ask, a mutation
        // must SUSPEND (create a pending approval) and then return the user's
        // interactive decision — not an immediate deny.
        let gate = Arc::new(CoworkGate::default()); // Ask by default.
        let dispatcher = Arc::new(Dispatcher::new(16));
        let g2 = Arc::clone(&gate);
        let d2 = Arc::clone(&dispatcher);
        let handle = tokio::spawn(async move {
            let decision = g2
                .gate_tool(
                    0,
                    "write_file",
                    json!({ "path": "x" }),
                    "",
                    Some((d2.as_ref(), None)),
                )
                .await;
            gate_response(&decision)
        });

        // The call must be pending (suspended), proving it did not deny immediately.
        let id = {
            let mut found = None;
            for _ in 0..10_000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("Ask mode must suspend on a pending approval, not deny immediately")
        };
        assert!(gate.approve(&id).await);
        let out = handle.await.unwrap();
        assert_eq!(
            out["decision"], "allow",
            "approving the suspended call must resolve to allow"
        );
    }

    #[tokio::test]
    async fn ask_mode_deny_resolves_to_deny() {
        let gate = Arc::new(CoworkGate::default());
        let dispatcher = Arc::new(Dispatcher::new(16));
        let g2 = Arc::clone(&gate);
        let d2 = Arc::clone(&dispatcher);
        let handle = tokio::spawn(async move {
            let decision = g2
                .gate_tool(
                    0,
                    "bash",
                    json!({ "command": "rm -rf /" }),
                    "",
                    Some((d2.as_ref(), None)),
                )
                .await;
            gate_response(&decision)
        });
        let id = {
            let mut found = None;
            for _ in 0..10_000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("Ask mode must suspend")
        };
        assert!(gate.deny(&id, "too dangerous".into()).await);
        let out = handle.await.unwrap();
        assert_eq!(out["decision"], "deny");
        assert_eq!(out["reason"], "too dangerous");
    }

    #[tokio::test]
    async fn read_only_tool_allows_without_suspending() {
        let gate = CoworkGate::default();
        let dispatcher = Dispatcher::new(16);
        let out = gate_response(
            &gate
                .gate_tool(0, "read_file", json!({}), "", Some((&dispatcher, None)))
                .await,
        );
        assert_eq!(out["decision"], "allow");
        assert!(gate.list_pending().await.is_empty());
    }

    #[tokio::test]
    async fn plan_mode_denies_mutation() {
        let gate = CoworkGate::default();
        gate.set_mode(PermissionMode::Plan).await;
        let dispatcher = Dispatcher::new(16);
        let out = gate_response(
            &gate
                .gate_tool(0, "write_file", json!({}), "", Some((&dispatcher, None)))
                .await,
        );
        assert_eq!(out["decision"], "deny");
    }
}
