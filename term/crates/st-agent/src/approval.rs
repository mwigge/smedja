//! Inline approval-gate state for a single pending tool call.

use serde_json::Value;

use crate::session::ApprovalState;

/// Inline approval-gate state for a single pending tool call.
///
/// Rendered by the terminal when an agent requests interactive approval.
pub struct ApprovalGate {
    /// Pane that owns this gate.
    pub pane_id: String,
    /// Name of the tool requesting approval.
    pub tool_name: String,
    /// Full argument object for the tool call.
    pub args: Value,
    /// Human-readable prompt from smdjad.
    pub prompt: String,
    /// Current approval state.
    pub state: ApprovalState,
}

impl ApprovalGate {
    /// Renders the approval gate as a list of display lines suitable for
    /// writing to the terminal.
    #[must_use]
    pub fn render_lines(&self) -> Vec<String> {
        let state_label = match self.state {
            ApprovalState::None | ApprovalState::Pending => "Pending",
            ApprovalState::Approved => "Approved",
            ApprovalState::Denied => "Denied",
        };
        let args_pretty =
            serde_json::to_string_pretty(&self.args).unwrap_or_else(|_| self.args.to_string());
        vec![
            format!("┌─ Approval required ─────────────────────────────"),
            format!("│  Tool   : {}", self.tool_name),
            format!("│  Prompt : {}", self.prompt),
            format!("│  Args   : {args_pretty}"),
            format!("│  State  : {state_label}"),
            format!("└─────────────────────────────────────────────────"),
        ]
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_gate_render_lines_contains_tool_name() {
        let gate = ApprovalGate {
            pane_id: "pane-1".into(),
            tool_name: "bash".into(),
            args: serde_json::json!({"cmd": "ls"}),
            prompt: "Allow bash?".into(),
            state: ApprovalState::Pending,
        };
        let lines = gate.render_lines();
        assert!(
            lines.iter().any(|l| l.contains("bash")),
            "render_lines must mention the tool name"
        );
    }
}
