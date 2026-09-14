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
    // Route through the real interactive gate: persisted [[permission.rules]]
    // and read-only/auto/accept-edits allow immediately, plan denies, and `Ask`
    // suspends (publishing a CoworkRequest to the TUI) until the user resolves
    // it — up to the gate's 30-min wait, which the hook's 1800s timeout mirrors.
    // A timeout or channel close fails closed. Raw tool_input goes in; the gate
    // scrubs the display copy.
    let workspace = hook_workspace(&params);
    let ctx = crate::cowork::GateContext {
        agent: Some("claude".to_owned()),
        workspace: Some(workspace.clone()),
        supports_modify: true,
    };
    let outcome = gate
        .gate_tool(
            0,
            &tool_name,
            tool_input,
            "",
            &ctx,
            Some((state.dispatcher.as_ref(), None)),
        )
        .await;
    // A modify rewrites the call: re-check the REPLACEMENT input against the
    // workspace's Deny rules so a human-edited call cannot smuggle past a
    // rule the original call would have hit.
    let decision = recheck_modified_decision(outcome.decision, &workspace, &tool_name);
    let mut resp = gate_response(&decision);
    if let Some(note) = outcome.note {
        resp["note"] = json!(note);
    }
    Ok(resp)
}

/// Re-checks a [`crate::cowork::Decision::Modify`]'s replacement input against
/// the workspace's Deny rules: a human-approved rewrite must not smuggle a
/// call a rule blocks (the original call was gated, the modified one was not).
/// Any other decision passes through unchanged. Kept pure so the policy is
/// unit-testable.
fn recheck_modified_decision(
    decision: crate::cowork::Decision,
    workspace: &std::path::Path,
    tool: &str,
) -> crate::cowork::Decision {
    if let crate::cowork::Decision::Modify(new_input) = &decision {
        if let Some(reason) = crate::cowork::modified_input_denied(workspace, tool, new_input) {
            return crate::cowork::Decision::Deny(reason);
        }
    }
    decision
}

/// Workspace the hook's tool call runs in: the `cwd` the hook reports, falling
/// back to the daemon's own workspace root when the hook omits it (older CLI
/// versions) or sends an empty string. A reported cwd is only honoured when it
/// canonicalizes to a path INSIDE the daemon's workspace — an arbitrary or
/// nonexistent cwd would let the hook pick which `.smedja/workspace.toml`
/// permission rules apply (e.g. an allow-everything file in /tmp).
fn hook_workspace(params: &Value) -> std::path::PathBuf {
    let daemon_root = crate::common::workspace_root();
    let Some(reported) = params
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return daemon_root;
    };
    let Ok(canonical) = std::path::Path::new(reported).canonicalize() else {
        tracing::warn!(
            cwd = %reported,
            "hook reported a cwd that does not resolve; using daemon workspace root"
        );
        return daemon_root;
    };
    if canonical.starts_with(&daemon_root) {
        canonical
    } else {
        tracing::warn!(
            cwd = %reported,
            root = %daemon_root.display(),
            "hook cwd is outside the daemon workspace; using daemon workspace root"
        );
        daemon_root
    }
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
/// Pass `scope: "always"` to resolve with an *allow-always* scope: the approval
/// resolves exactly as a normal approve, and the resolution site — the gate's
/// shared suspend path (native loop, claude hook, ACP tool-gate adapter) or the
/// industry-ACP `session/request_permission` bridge — persists a matching
/// `[[permission.rules]]` Allow entry so the choice sticks across turns and
/// backends. Any other `scope` (or none) is an ordinary one-shot approve.
///
/// `scope: "always"` additionally returns `note` when the allow-always could
/// not be persisted (see [`crate::cowork::allow_always_skip_reason`]) so the
/// caller can tell the user the choice applies to this call only.
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
    let always = params
        .get("scope")
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("always"));
    let gate = gate_for(&state, &session_id).await?;
    let (found, note) = if always {
        gate.approve_always(&id).await
    } else {
        (gate.approve(&id).await, None)
    };
    let mut resp = json!({ "id": id, "resolved": found });
    if let Some(note) = note {
        resp["note"] = json!(note);
    }
    Ok(resp)
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

/// Handles `cowork.modify`: resolves a pending approval with replacement
/// arguments. The instruction REPLACES the tool input and must parse as a JSON
/// object — anything else (and backends without a modify channel) is rejected
/// with an RPC error so the TUI can show it, leaving the prompt pending for a
/// corrected decision rather than silently degrading to a deny.
///
/// # Errors
///
/// Returns an error when `session_id`/`id` is missing, no gate is registered,
/// or the modify is rejected (unknown id, unsupported backend, or an
/// instruction that is not a JSON object of replacement arguments).
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
    gate.modify(&id, instruction)
        .await
        .map_err(|e| RpcError::new(codes::INVALID_PARAMS, e.to_string()))?;
    Ok(json!({ "id": id, "resolved": true }))
}

/// Handles `cowork.resolve`: the terminal push-socket client's answer to an
/// agent-events `ApprovalPrompt` — params are `{approval_id, approved}`, both
/// required.
///
/// The sender only knows the approval id (not which session's gate holds it —
/// approval ids are UUIDs, globally unique), so the id is resolved against
/// every registered gate, reusing the same [`CoworkGate::approve`] /
/// [`CoworkGate::deny`] resolution path as `cowork.approve` / `cowork.deny`.
///
/// With `approved: true` and `scope: "always"` the resolution goes through
/// [`CoworkGate::approve_always`] instead — the same allow-always semantics as
/// `cowork.approve` (the suspended resolution site persists a matching
/// `[[permission.rules]]` Allow entry) — and the response carries `note` when
/// persistence was vetoed ([`crate::cowork::allow_always_skip_reason`]). Any
/// other `scope` (or none), and any `scope` with `approved: false`, is an
/// ordinary one-shot resolve.
///
/// # Errors
///
/// Returns an error when `approval_id` or `approved` is missing. An unknown id
/// is **not** an error — the prompt may already be resolved or timed out —
/// and answers `{resolved: false}`.
pub(crate) async fn resolve(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let id = params
        .get("approval_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("approval_id"))?
        .to_owned();
    // Required: a missing `approved` must not silently mean "deny".
    let approved = params
        .get("approved")
        .and_then(Value::as_bool)
        .ok_or_else(|| missing_param("approved"))?;
    let always = params
        .get("scope")
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("always"));
    // Snapshot the gate map (Arc clones) so the lock is not held across the
    // resolution awaits.
    let gates = state.gates.lock().await.clone();
    let (resolved, note) = resolve_approval_by_id(&gates, &id, approved, always).await;
    let mut resp = json!({ "id": id, "resolved": resolved });
    if let Some(note) = note {
        resp["note"] = json!(note);
    }
    Ok(resp)
}

/// Resolves `id` against every registered gate (see [`resolve`]), reusing the
/// same [`CoworkGate::approve`] / [`CoworkGate::deny`] path as
/// `cowork.approve` / `cowork.deny` — or [`CoworkGate::approve_always`] when
/// `always` is set on an approval (a deny is never allow-always). Returns
/// `(resolved, note)`: `resolved` is `true` when some gate held the id; `note`
/// carries the allow-always persistence veto, if any.
async fn resolve_approval_by_id(
    gates: &std::collections::HashMap<String, Arc<CoworkGate>>,
    id: &str,
    approved: bool,
    always: bool,
) -> (bool, Option<String>) {
    for gate in gates.values() {
        let (found, note) = if always && approved {
            gate.approve_always(id).await
        } else if approved {
            (gate.approve(id).await, None)
        } else {
            (gate.deny(id, "denied from terminal".to_owned()).await, None)
        };
        if found {
            return (true, note);
        }
    }
    (false, None)
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
                "agent": p.agent,
                "cwd": p.cwd,
                "risk": crate::cowork::tool_risk(&p.tool),
                "supports_modify": p.supports_modify,
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
                    &crate::cowork::GateContext::default(),
                    Some((d2.as_ref(), None)),
                )
                .await;
            gate_response(&decision.decision)
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
                    &crate::cowork::GateContext::default(),
                    Some((d2.as_ref(), None)),
                )
                .await;
            gate_response(&decision.decision)
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
                .gate_tool(
                    0,
                    "read_file",
                    json!({}),
                    "",
                    &crate::cowork::GateContext::default(),
                    Some((&dispatcher, None)),
                )
                .await
                .decision,
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
                .gate_tool(
                    0,
                    "write_file",
                    json!({}),
                    "",
                    &crate::cowork::GateContext::default(),
                    Some((&dispatcher, None)),
                )
                .await
                .decision,
        );
        assert_eq!(out["decision"], "deny");
    }

    #[tokio::test]
    async fn hook_path_honors_persisted_allow_rule() {
        // An allow-always rule persisted earlier must short-circuit the hook
        // path too — not just the native loop — so repeat prompts stop.
        let ws = tempfile::tempdir().unwrap();
        crate::cowork::append_allow_rule(ws.path(), "bash", &json!({"command": "git status"}))
            .unwrap();
        let gate = CoworkGate::default(); // Ask mode — would suspend without the rule.
        let dispatcher = Dispatcher::new(16);
        let ctx = crate::cowork::GateContext {
            agent: Some("claude".to_owned()),
            workspace: Some(ws.path().to_path_buf()),
            supports_modify: true,
        };
        let out = gate_response(
            &gate
                .gate_tool(
                    0,
                    "bash",
                    json!({"command": "git status"}),
                    "",
                    &ctx,
                    Some((&dispatcher, None)),
                )
                .await
                .decision,
        );
        assert_eq!(out["decision"], "allow");
        assert!(gate.list_pending().await.is_empty());
    }

    // ── modify re-check ──────────────────────────────────────────────────────

    #[test]
    fn recheck_modified_decision_denies_rule_blocked_rewrite() {
        use crate::cowork::Decision;
        let ws = tempfile::tempdir().unwrap();
        let dir = ws.path().join(".smedja");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("workspace.toml"),
            "[[permission.rules]]\ntool = \"bash\"\ncommand_pattern = \"rm *\"\nmode = \"deny\"\n",
        )
        .unwrap();

        // A rewrite that hits the deny rule is re-blocked...
        let denied = super::recheck_modified_decision(
            Decision::Modify("{\"command\": \"rm -rf /tmp/x\"}".into()),
            ws.path(),
            "bash",
        );
        assert!(
            matches!(denied, Decision::Deny(ref r) if r.contains("permission rule")),
            "modified call matching a deny rule must be denied: {denied:?}"
        );
        // ...a clean rewrite passes through untouched...
        let ok = super::recheck_modified_decision(
            Decision::Modify("{\"command\": \"ls\"}".into()),
            ws.path(),
            "bash",
        );
        assert!(matches!(ok, Decision::Modify(_)));
        // ...and non-modify decisions never consult the rules.
        let plain = super::recheck_modified_decision(Decision::Approve, ws.path(), "bash");
        assert!(matches!(plain, Decision::Approve));
    }

    // ── cowork.resolve mapping ───────────────────────────────────────────────

    #[tokio::test]
    async fn cowork_resolve_approved_resolves_pending() {
        let gate = Arc::new(CoworkGate::default());
        let mut gates = std::collections::HashMap::new();
        gates.insert("sess-1".to_owned(), Arc::clone(&gate));
        let g2 = Arc::clone(&gate);
        let handle = tokio::spawn(async move {
            g2.intercept(
                crate::cowork::ApprovalPrompt::new(1, "bash", &json!({"cmd": "ls"}), ""),
                0,
                None,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let id = gate.list_pending().await[0].0.clone();

        assert!(
            super::resolve_approval_by_id(&gates, &id, true, false)
                .await
                .0
        );
        assert!(matches!(handle.await.unwrap(), Decision::Approve));
        assert!(gate.list_pending().await.is_empty());
    }

    #[tokio::test]
    async fn cowork_resolve_denied_resolves_with_reason() {
        let gate = Arc::new(CoworkGate::default());
        let mut gates = std::collections::HashMap::new();
        gates.insert("sess-1".to_owned(), Arc::clone(&gate));
        let g2 = Arc::clone(&gate);
        let handle = tokio::spawn(async move {
            g2.intercept(
                crate::cowork::ApprovalPrompt::new(1, "bash", &json!({"cmd": "ls"}), ""),
                0,
                None,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let id = gate.list_pending().await[0].0.clone();

        assert!(
            super::resolve_approval_by_id(&gates, &id, false, false)
                .await
                .0
        );
        assert!(matches!(
            handle.await.unwrap(),
            Decision::Deny(ref r) if r == "denied from terminal"
        ));
    }

    #[tokio::test]
    async fn cowork_resolve_unknown_id_resolves_nothing() {
        let gate = Arc::new(CoworkGate::default());
        let mut gates = std::collections::HashMap::new();
        gates.insert("sess-1".to_owned(), gate);
        assert!(
            !super::resolve_approval_by_id(&gates, "no-such-id", true, false)
                .await
                .0
        );
    }

    #[tokio::test]
    async fn cowork_resolve_scope_always_persists_allow_rule() {
        // The TUI's allow-always key sends cowork.resolve with scope "always"
        // and no session id; the daemon must persist the same Allow rule as
        // cowork.approve with scope "always".
        let ws = tempfile::tempdir().unwrap();
        let gate = Arc::new(CoworkGate::default());
        let mut gates = std::collections::HashMap::new();
        gates.insert("sess-1".to_owned(), Arc::clone(&gate));
        let ctx = crate::cowork::GateContext {
            agent: Some("claude".to_owned()),
            workspace: Some(ws.path().to_path_buf()),
            supports_modify: true,
        };
        let g2 = Arc::clone(&gate);
        let handle = tokio::spawn(async move {
            g2.gate_tool(0, "bash", json!({"command": "git status"}), "", &ctx, None)
                .await
        });
        let id = {
            let mut found = None;
            for _ in 0..1000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("pending approval should appear")
        };

        let (resolved, note) = super::resolve_approval_by_id(&gates, &id, true, true).await;
        assert!(resolved);
        assert!(note.is_none(), "a clean command must persist: {note:?}");
        assert!(matches!(handle.await.unwrap().decision, Decision::Approve));
        let rules = crate::cowork::load_permission_rules(ws.path());
        assert_eq!(rules.len(), 1, "scope=always must persist an allow rule");
        assert_eq!(rules[0].tool, "bash");
        assert_eq!(rules[0].command_pattern.as_deref(), Some("git status"));
    }

    #[tokio::test]
    async fn cowork_resolve_scope_always_with_glob_command_carries_veto_note() {
        // A globby command cannot be scoped into a rule: the approval stands
        // once, nothing is persisted, and the note says why.
        let ws = tempfile::tempdir().unwrap();
        let gate = Arc::new(CoworkGate::default());
        let mut gates = std::collections::HashMap::new();
        gates.insert("sess-1".to_owned(), Arc::clone(&gate));
        let ctx = crate::cowork::GateContext {
            agent: Some("claude".to_owned()),
            workspace: Some(ws.path().to_path_buf()),
            supports_modify: true,
        };
        let g2 = Arc::clone(&gate);
        let handle = tokio::spawn(async move {
            g2.gate_tool(0, "bash", json!({"command": "rm -rf *"}), "", &ctx, None)
                .await
        });
        let id = {
            let mut found = None;
            for _ in 0..1000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("pending approval should appear")
        };

        let (resolved, note) = super::resolve_approval_by_id(&gates, &id, true, true).await;
        assert!(resolved);
        let note = note.expect("a glob command must carry the persistence veto note");
        assert!(note.contains("glob"), "unexpected veto note: {note}");
        let outcome = handle.await.unwrap();
        assert!(matches!(outcome.decision, Decision::Approve));
        assert!(
            outcome.note.is_some(),
            "the gate outcome must also carry the veto note"
        );
        assert!(
            crate::cowork::load_permission_rules(ws.path()).is_empty(),
            "no rule may be persisted for a glob command"
        );
    }

    #[tokio::test]
    async fn cowork_resolve_scope_always_on_deny_is_plain_deny() {
        // scope "always" only applies to approvals; a deny stays a plain deny.
        let gate = Arc::new(CoworkGate::default());
        let mut gates = std::collections::HashMap::new();
        gates.insert("sess-1".to_owned(), Arc::clone(&gate));
        let g2 = Arc::clone(&gate);
        let handle = tokio::spawn(async move {
            g2.intercept(
                crate::cowork::ApprovalPrompt::new(1, "bash", &json!({"cmd": "ls"}), ""),
                0,
                None,
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let id = gate.list_pending().await[0].0.clone();

        let (resolved, note) = super::resolve_approval_by_id(&gates, &id, false, true).await;
        assert!(resolved);
        assert!(note.is_none());
        assert!(matches!(
            handle.await.unwrap(),
            Decision::Deny(ref r) if r == "denied from terminal"
        ));
        assert!(
            !gate.take_always(&id).await,
            "a deny must not set the always flag"
        );
    }

    // ── hook workspace resolution ────────────────────────────────────────────

    #[test]
    fn hook_workspace_prefers_reported_cwd_inside_workspace() {
        let root = crate::common::workspace_root();
        // A subdirectory of the daemon's own workspace is honoured.
        let params = json!({ "cwd": root.join("bin").display().to_string() });
        let resolved = super::hook_workspace(&params);
        assert!(
            resolved.ends_with("bin"),
            "a cwd inside the daemon workspace must be honoured, got {}",
            resolved.display()
        );
    }

    #[test]
    fn hook_workspace_rejects_cwd_outside_workspace() {
        let fallback = crate::common::workspace_root();
        // An absolute path outside the daemon workspace must not steer rule
        // lookup (e.g. an allow-everything .smedja/workspace.toml in /tmp).
        assert_eq!(super::hook_workspace(&json!({ "cwd": "/tmp" })), fallback);
    }

    #[test]
    fn hook_workspace_rejects_unresolvable_cwd() {
        let fallback = crate::common::workspace_root();
        assert_eq!(
            super::hook_workspace(&json!({ "cwd": "/nonexistent/dir/xyz" })),
            fallback
        );
    }

    #[test]
    fn hook_workspace_falls_back_on_missing_or_empty_cwd() {
        let fallback = crate::common::workspace_root();
        assert_eq!(super::hook_workspace(&json!({})), fallback);
        assert_eq!(super::hook_workspace(&json!({ "cwd": "" })), fallback);
    }
}
