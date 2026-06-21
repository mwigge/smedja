//! Human-in-the-loop gate for tool calls in cowork mode.

use serde::{Deserialize, Serialize};

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

/// Intercepts tool calls when cowork mode is active.
///
/// In this initial implementation the gate records the approval prompt and
/// returns an immediate auto-approve (full interactive approval loop requires
/// the TUI event stream and is deferred to a later phase).
///
/// # ponytail: auto-approve stub; interactive gate wired once TUI event stream is available
pub struct CoworkGate;

impl CoworkGate {
    /// Intercepts a tool call. Returns the resolved [`Decision`].
    ///
    /// Currently always returns [`Decision::Approve`] after logging the prompt.
    pub fn intercept(&self, prompt: &ApprovalPrompt) -> Decision {
        tracing::info!(
            tool = %prompt.tool,
            step = prompt.step_n,
            "cowork gate: auto-approving (interactive gate not yet wired)"
        );
        Decision::Approve
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn intercept_returns_approve() {
        let gate = CoworkGate;
        let prompt = ApprovalPrompt {
            step_n: 1,
            tool: "bash".into(),
            args_scrubbed: json!({"cmd": "ls"}),
            reasoning: "list files".into(),
            plan_summary: "exploration".into(),
        };
        assert!(matches!(gate.intercept(&prompt), Decision::Approve));
    }
}
