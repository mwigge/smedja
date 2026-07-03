//! The `CoworkGate`: intercepts tool calls and suspends for human decisions.

use std::{collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize};
use smedja_bellows::{CorrelationCtx, Dispatcher, TurnEvent};
use tokio::sync::{oneshot, Mutex};

use super::policy::{evaluate, PermissionDecision, PermissionMode};

/// Describes a pending tool call awaiting human approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalPrompt {
    pub step_n: u32,
    pub tool: String,
    /// Args with any secret values scrubbed.
    pub args_scrubbed: serde_json::Value,
    pub reasoning: String,
    pub plan_summary: String,
}

/// The human's decision on a pending tool call.
#[derive(Debug, Clone)]
pub enum Decision {
    Approve,
    Deny(String),
    Modify(String),
}

/// Unique ID for a pending approval request.
pub type ApprovalId = String;

/// A pending approval awaiting a human decision.
struct PendingApproval {
    prompt: ApprovalPrompt,
    /// Sender half of the oneshot; the receiver suspends in [`CoworkGate::intercept`].
    tx: oneshot::Sender<Decision>,
}

/// Intercepts tool calls when cowork mode is active.
///
/// One `CoworkGate` per session. External RPC calls (`cowork.approve`,
/// `cowork.deny`, `cowork.modify`) send decisions through the channel.
///
/// Codex-backed sessions that manage their own approval loop skip `intercept`
/// entirely at the call site rather than using a bypass flag on the gate.
#[derive(Default)]
pub struct CoworkGate {
    pending: Arc<Mutex<HashMap<ApprovalId, PendingApproval>>>,
    /// Per-session permission mode driving the gate policy (Shift+Tab cycles it
    /// from the TUI). Defaults to [`PermissionMode::Ask`].
    mode: Arc<Mutex<PermissionMode>>,
}

impl CoworkGate {
    /// Submits a tool call for approval. Suspends until a decision arrives
    /// or the optional `timeout_secs` (0 = infinite) elapses.
    ///
    /// If `push` is `Some((dispatcher, turn_id))`, a [`TurnEvent::CoworkRequest`]
    /// is published immediately after registering the pending approval so the TUI
    /// receives the request via the NDJSON stream instead of polling.
    ///
    /// Returns [`Decision::Deny`] on timeout or channel close (fail-closed).
    pub async fn intercept(
        &self,
        prompt: ApprovalPrompt,
        timeout_secs: u64,
        push: Option<(&Dispatcher, Option<&str>)>,
    ) -> Decision {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(
                id.clone(),
                PendingApproval {
                    prompt: prompt.clone(),
                    tx,
                },
            );
        }
        if let Some((dispatcher, turn_id)) = push {
            dispatcher.publish(TurnEvent::CoworkRequest {
                approval_id: id.clone(),
                tool: prompt.tool.clone(),
                step_n: prompt.step_n,
                args_display: prompt.args_scrubbed.to_string(),
                reasoning: prompt.reasoning.clone(),
                turn_id: turn_id.map(str::to_owned),
                correlation: CorrelationCtx::default(),
            });
        }
        tracing::info!(
            approval_id = %id,
            tool = %prompt.tool,
            step = prompt.step_n,
            "cowork gate: awaiting human decision",
        );

        if timeout_secs == 0 {
            // Wait indefinitely; deny if the channel closes unexpectedly.
            rx.await
                .unwrap_or_else(|_| Decision::Deny("channel closed".to_owned()))
        } else {
            match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await {
                Ok(Ok(decision)) => decision,
                Ok(Err(_)) => {
                    // Sender dropped without sending — deny.
                    Decision::Deny("channel closed".to_owned())
                }
                Err(_) => {
                    tracing::warn!(
                        approval_id = %id,
                        timeout_secs,
                        "cowork gate: approval timed out; denying",
                    );
                    Decision::Deny("timeout".to_owned())
                }
            }
        }
    }

    /// Resolves a pending approval with [`Decision::Approve`].
    ///
    /// Returns `true` if the approval ID was found and resolved.
    pub async fn approve(&self, id: &str) -> bool {
        self.resolve(id, Decision::Approve).await
    }

    /// Resolves a pending approval with [`Decision::Deny`].
    ///
    /// Returns `true` if the approval ID was found and resolved.
    pub async fn deny(&self, id: &str, reason: String) -> bool {
        self.resolve(id, Decision::Deny(reason)).await
    }

    /// Resolves a pending approval with [`Decision::Modify`].
    ///
    /// Returns `true` if the approval ID was found and resolved.
    pub async fn modify(&self, id: &str, instruction: String) -> bool {
        self.resolve(id, Decision::Modify(instruction)).await
    }

    /// Lists pending approvals with their full prompts, ordered by insertion UUID
    /// (arbitrary but stable within a poll interval).
    pub async fn list_pending(&self) -> Vec<(ApprovalId, ApprovalPrompt)> {
        self.pending
            .lock()
            .await
            .iter()
            .map(|(id, p)| (id.clone(), p.prompt.clone()))
            .collect()
    }

    /// Gates a single tool call under the gate's current [`PermissionMode`]:
    /// allow/deny outright per [`evaluate`], or — for `Ask` — suspend on the
    /// gate (≤30 min) until the user decides. Returns the resolved [`Decision`].
    ///
    /// Pass `push` to have a [`TurnEvent::CoworkRequest`] pushed via the NDJSON
    /// stream so the TUI receives it without polling.
    pub async fn gate_tool(
        &self,
        step_n: u32,
        tool: &str,
        args_scrubbed: serde_json::Value,
        reasoning: &str,
        push: Option<(&Dispatcher, Option<&str>)>,
    ) -> Decision {
        let mode = self.mode().await;
        match evaluate(mode, tool) {
            PermissionDecision::Allow => Decision::Approve,
            PermissionDecision::Deny => {
                Decision::Deny(format!("blocked by {} mode", mode.as_str()))
            }
            PermissionDecision::Ask => {
                self.intercept(
                    ApprovalPrompt {
                        step_n,
                        tool: tool.to_owned(),
                        args_scrubbed,
                        reasoning: reasoning.to_owned(),
                        plan_summary: String::new(),
                    },
                    30 * 60,
                    push,
                )
                .await
            }
        }
    }

    /// Like [`Self::gate_tool`] but always suspends for a human decision,
    /// ignoring the mode's allow/auto — for high-risk roles (`IaC`) whose
    /// mutations must be confirmed even under `AcceptEdits`/`Auto`.
    pub async fn gate_tool_forced_ask(
        &self,
        step_n: u32,
        tool: &str,
        args_scrubbed: serde_json::Value,
        reasoning: &str,
        push: Option<(&Dispatcher, Option<&str>)>,
    ) -> Decision {
        self.intercept(
            ApprovalPrompt {
                step_n,
                tool: tool.to_owned(),
                args_scrubbed,
                reasoning: reasoning.to_owned(),
                plan_summary: String::new(),
            },
            30 * 60,
            push,
        )
        .await
    }

    /// The gate's current permission mode.
    pub async fn mode(&self) -> PermissionMode {
        *self.mode.lock().await
    }

    /// Sets the permission mode; returns the new value.
    pub async fn set_mode(&self, mode: PermissionMode) -> PermissionMode {
        *self.mode.lock().await = mode;
        mode
    }

    /// Cycles to the next permission mode (Shift+Tab); returns the new value.
    pub async fn cycle_mode(&self) -> PermissionMode {
        let mut m = self.mode.lock().await;
        *m = m.next();
        *m
    }

    async fn resolve(&self, id: &str, decision: Decision) -> bool {
        let mut pending = self.pending.lock().await;
        if let Some(entry) = pending.remove(id) {
            let _ = entry.tx.send(decision);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn prompt() -> ApprovalPrompt {
        ApprovalPrompt {
            step_n: 1,
            tool: "bash".into(),
            args_scrubbed: json!({"cmd": "ls"}),
            reasoning: "list files".into(),
            plan_summary: "exploration".into(),
        }
    }

    #[tokio::test]
    async fn gate_tool_allow_deny_and_ask_paths() {
        let gate = CoworkGate::default(); // Ask mode by default.
                                          // Read-only: allowed, no pending entry.
        assert!(matches!(
            gate.gate_tool(1, "read_file", json!({}), "", None).await,
            Decision::Approve
        ));
        assert!(gate.list_pending().await.is_empty());

        // Plan mode denies a write outright.
        gate.set_mode(PermissionMode::Plan).await;
        assert!(matches!(
            gate.gate_tool(1, "write_file", json!({}), "", None).await,
            Decision::Deny(_)
        ));

        // Ask mode suspends; approving concurrently resolves it.
        let gate = Arc::new(CoworkGate::default());
        let g2 = Arc::clone(&gate);
        let h = tokio::spawn(async move {
            g2.gate_tool(1, "write_file", json!({ "path": "x" }), "edit", None)
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
        assert!(gate.approve(&id).await);
        assert!(matches!(h.await.unwrap(), Decision::Approve));
    }

    #[tokio::test]
    async fn approve_resolves_pending() {
        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);

        let handle = tokio::spawn(async move { gate2.intercept(prompt(), 0, None).await });

        // Give the intercept task time to register itself.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let pending = gate.list_pending().await;
        assert_eq!(pending.len(), 1);
        let id = pending[0].0.clone();

        assert!(gate.approve(&id).await);
        let decision = handle.await.unwrap();
        assert!(matches!(decision, Decision::Approve));
    }

    #[tokio::test]
    async fn deny_resolves_with_reason() {
        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);

        let handle = tokio::spawn(async move { gate2.intercept(prompt(), 0, None).await });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let pending = gate.list_pending().await;
        let id = pending[0].0.clone();

        assert!(gate.deny(&id, "too risky".into()).await);
        let decision = handle.await.unwrap();
        assert!(matches!(decision, Decision::Deny(r) if r == "too risky"));
    }

    #[tokio::test]
    async fn timeout_denies() {
        let gate = CoworkGate::default();
        let decision = gate.intercept(prompt(), 1, None).await;
        assert!(matches!(decision, Decision::Deny(r) if r == "timeout"));
    }

    #[tokio::test]
    async fn unknown_id_resolve_returns_false() {
        let gate = CoworkGate::default();
        assert!(!gate.approve("nonexistent-id").await);
        assert!(!gate.deny("nonexistent-id", "reason".into()).await);
        assert!(!gate.modify("nonexistent-id", "instruction".into()).await);
    }

    /// Session-skip path: when a Codex-backed session calls intercept but the
    /// caller is responsible for skipping intercept entirely, the gate itself
    /// still works correctly — approve resolves immediately.
    #[tokio::test]
    async fn session_skip_approve_resolves() {
        // Callers that want to skip the gate simply don't call intercept.
        // This test exercises that the gate resolves correctly when used directly,
        // which is all we can assert from outside the call site.
        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);

        let handle = tokio::spawn(async move { gate2.intercept(prompt(), 0, None).await });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let pending = gate.list_pending().await;
        assert_eq!(pending.len(), 1, "one pending approval expected");
        let id = pending[0].0.clone();
        gate.approve(&id).await;

        let decision = handle.await.unwrap();
        assert!(matches!(decision, Decision::Approve));
        assert!(gate.list_pending().await.is_empty());
    }

    #[tokio::test]
    async fn approval_round_trip_emits_pending_then_resolves() {
        let gate = Arc::new(CoworkGate::default());
        let gate_ref = Arc::clone(&gate);

        // Spawn a task that intercepts a tool call.
        let intercept_handle =
            tokio::spawn(async move { gate_ref.intercept(prompt(), 5, None).await });

        // Give intercept time to register the pending entry.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Verify the pending entry is visible.
        let pending = gate.list_pending().await;
        assert_eq!(pending.len(), 1, "expected one pending approval");
        let id = pending[0].0.clone();

        // Approve it.
        let resolved = gate.approve(&id).await;
        assert!(resolved, "approve must return true for a known id");

        // The intercepting task should now resolve to Approve.
        let decision = intercept_handle.await.expect("intercept task panicked");
        assert!(
            matches!(decision, Decision::Approve),
            "expected Decision::Approve after approval"
        );
    }

    #[tokio::test]
    async fn intercept_emits_pending_for_any_runner() {
        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);

        let handle = tokio::spawn(async move { gate2.intercept(prompt(), 0, None).await });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let pending = gate.list_pending().await;
        assert_eq!(
            pending.len(),
            1,
            "intercept must create a pending entry for any runner"
        );
        assert_eq!(pending[0].1.tool, "bash");

        // Clean up: approve so the spawned task can finish.
        let id = pending[0].0.clone();
        gate.approve(&id).await;
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn intercept_push_publishes_cowork_request_event() {
        use smedja_bellows::Dispatcher;

        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);
        let dispatcher = Arc::new(Dispatcher::new(16));
        let mut rx = dispatcher.subscribe();
        let disp_ref = Arc::clone(&dispatcher);

        let handle = tokio::spawn(async move {
            gate2
                .intercept(prompt(), 0, Some((disp_ref.as_ref(), Some("t-99"))))
                .await
        });

        // The CoworkRequest event must arrive before the gate suspends.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let event = rx.try_recv().expect("CoworkRequest must be published");
        let smedja_bellows::TurnEvent::CoworkRequest {
            ref tool,
            ref turn_id,
            ..
        } = event
        else {
            panic!("expected CoworkRequest, got {event:?}");
        };
        assert_eq!(tool, "bash");
        assert_eq!(turn_id.as_deref(), Some("t-99"));

        // Clean up.
        let pending = gate.list_pending().await;
        gate.approve(&pending[0].0).await;
        handle.await.unwrap();
    }
}
