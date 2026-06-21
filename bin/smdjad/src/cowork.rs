//! Human-in-the-loop gate for tool calls in cowork mode.

use std::{collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex};

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
#[derive(Default)]
pub struct CoworkGate {
    pending: Arc<Mutex<HashMap<ApprovalId, PendingApproval>>>,
    /// When `true`, [`CoworkGate::intercept`] returns [`Decision::Approve`] immediately.
    ///
    /// Used for Codex-backed sessions that manage their own approval loop.
    bypass: bool,
}

impl CoworkGate {
    /// Creates a new gate with no pending approvals.
    pub fn new() -> Self {
        Self::default()
    }

    /// Disables the gate for this session — all `intercept` calls return `Decision::Approve` immediately.
    ///
    /// Use for Codex-backed sessions that manage their own approval loop.
    pub fn set_bypass(&mut self, bypass: bool) {
        self.bypass = bypass;
    }

    // ponytail: approval gate applies to all runners; Codex is excluded via set_bypass

    /// Submits a tool call for approval. Suspends until a decision arrives
    /// or the optional `timeout_secs` (0 = infinite) elapses.
    ///
    /// Returns [`Decision::Approve`] on timeout (with a WARN log).
    pub async fn intercept(&self, prompt: ApprovalPrompt, timeout_secs: u64) -> Decision {
        if self.bypass {
            return Decision::Approve;
        }
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
        tracing::info!(
            approval_id = %id,
            tool = %prompt.tool,
            step = prompt.step_n,
            "cowork gate: awaiting human decision",
        );

        if timeout_secs == 0 {
            rx.await.unwrap_or(Decision::Approve)
        } else if let Ok(Ok(decision)) =
            tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await
        {
            decision
        } else {
            tracing::warn!(
                approval_id = %id,
                "cowork gate: timeout, auto-approving",
            );
            Decision::Approve
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

    /// Lists pending approval IDs and their tool names.
    pub async fn list_pending(&self) -> Vec<(ApprovalId, String)> {
        self.pending
            .lock()
            .await
            .iter()
            .map(|(id, p)| (id.clone(), p.prompt.tool.clone()))
            .collect()
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
    async fn approve_resolves_pending() {
        let gate = Arc::new(CoworkGate::new());
        let gate2 = Arc::clone(&gate);

        let handle = tokio::spawn(async move { gate2.intercept(prompt(), 0).await });

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
        let gate = Arc::new(CoworkGate::new());
        let gate2 = Arc::clone(&gate);

        let handle = tokio::spawn(async move { gate2.intercept(prompt(), 0).await });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let pending = gate.list_pending().await;
        let id = pending[0].0.clone();

        assert!(gate.deny(&id, "too risky".into()).await);
        let decision = handle.await.unwrap();
        assert!(matches!(decision, Decision::Deny(r) if r == "too risky"));
    }

    #[tokio::test]
    async fn timeout_auto_approves() {
        let gate = CoworkGate::new();
        let decision = gate.intercept(prompt(), 1).await;
        assert!(matches!(decision, Decision::Approve));
    }

    #[tokio::test]
    async fn unknown_id_resolve_returns_false() {
        let gate = CoworkGate::new();
        assert!(!gate.approve("nonexistent-id").await);
        assert!(!gate.deny("nonexistent-id", "reason".into()).await);
        assert!(!gate.modify("nonexistent-id", "instruction".into()).await);
    }

    #[tokio::test]
    async fn bypass_auto_approves() {
        let mut gate = CoworkGate::new();
        gate.set_bypass(true);
        let decision = gate.intercept(prompt(), 0).await;
        assert!(matches!(decision, Decision::Approve));
        // No pending entry should have been created.
        assert!(gate.list_pending().await.is_empty());
    }

    #[tokio::test]
    async fn intercept_emits_pending_for_any_runner() {
        let gate = Arc::new(CoworkGate::new());
        let gate2 = Arc::clone(&gate);

        let handle = tokio::spawn(async move { gate2.intercept(prompt(), 0).await });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let pending = gate.list_pending().await;
        assert_eq!(
            pending.len(),
            1,
            "intercept must create a pending entry for any runner"
        );
        assert_eq!(pending[0].1, "bash");

        // Clean up: approve so the spawned task can finish.
        let id = pending[0].0.clone();
        gate.approve(&id).await;
        handle.await.unwrap();
    }
}
