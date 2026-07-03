//! Permission modes, tool risk classification, and the pure gate policy.

use serde::{Deserialize, Serialize};

/// Per-session permission mode controlling how mutating tool calls are gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Stop and ask before every mutation (edit/write/shell). The default.
    #[default]
    Ask,
    /// Auto-approve known file edits; still ask before shell/unknown tools.
    AcceptEdits,
    /// Read-only: deny all mutations (the agent may only read/analyse/plan).
    Plan,
    /// Auto-approve everything (no gate).
    Auto,
}

impl PermissionMode {
    /// Parses a mode name leniently; anything unrecognised falls back to `Ask`.
    #[must_use]
    pub fn parse_lenient(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "accept_edits" | "acceptedits" | "edits" => Self::AcceptEdits,
            "plan" => Self::Plan,
            "auto" => Self::Auto,
            _ => Self::Ask,
        }
    }

    /// Stable lowercase identifier (round-trips with [`Self::parse_lenient`]).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AcceptEdits => "accept_edits",
            Self::Plan => "plan",
            Self::Auto => "auto",
        }
    }

    /// Next mode in the `Shift+Tab` cycle: `Ask` → `AcceptEdits` → `Plan` → `Auto` → `Ask`.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Ask => Self::AcceptEdits,
            Self::AcceptEdits => Self::Plan,
            Self::Plan => Self::Auto,
            Self::Auto => Self::Ask,
        }
    }
}

/// The policy's verdict for a single tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Run the tool without asking.
    Allow,
    /// Block the tool outright (e.g. a mutation in `Plan` mode).
    Deny,
    /// Suspend on the cowork gate for a human decision.
    Ask,
}

/// Coarse risk class of a tool, by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    /// Read-only (never gated).
    ReadOnly,
    /// A known file mutation (auto-approved in `AcceptEdits`).
    Edit,
    /// Shell/command execution *or* an unknown tool — always needs explicit
    /// approval outside `Auto` (fail-safe: unknown tools are treated as exec).
    Exec,
}

fn tool_kind(tool: &str) -> ToolKind {
    let t = tool.to_ascii_lowercase();
    // Shell / arbitrary command execution — the most dangerous class.
    if t.contains("bash")
        || t.contains("shell")
        || t.contains("run_command")
        || t == "exec"
        || t.starts_with("exec_")
    {
        return ToolKind::Exec;
    }
    // Read-only tools (the daemon's read-safe set plus common read verbs).
    #[allow(clippy::items_after_statements)]
    const READ: &[&str] = &[
        "read_file",
        "list_files",
        "smedja_vault_search",
        "smedja_retrieve",
        "graph_query",
        "otel_query",
        "metric_query",
        "log_tail",
    ];
    if READ.contains(&t.as_str())
        || t.starts_with("read")
        || t.starts_with("list")
        || t.starts_with("get")
        || t.starts_with("search")
        || t.starts_with("query")
        || t.starts_with("grep")
        || t.starts_with("glob")
        || t.starts_with("view")
    {
        return ToolKind::ReadOnly;
    }
    // Known mutating edit tools.
    #[allow(clippy::items_after_statements)]
    const EDIT: &[&str] = &[
        "write_file",
        "edit_file",
        "smedja_vault_store",
        "apply_patch",
        "str_replace",
        "create_file",
        "delete_file",
        "write",
        "edit",
        "patch",
    ];
    if EDIT.contains(&t.as_str()) {
        return ToolKind::Edit;
    }
    // Unknown → conservative: treat as exec so it is never auto-approved by
    // AcceptEdits.
    ToolKind::Exec
}

/// Evaluates the permission decision for a tool call under `mode`. Pure; the
/// blocking/asking happens in [`crate::cowork::CoworkGate::gate_tool`].
#[must_use]
pub fn evaluate(mode: PermissionMode, tool: &str) -> PermissionDecision {
    match (mode, tool_kind(tool)) {
        (_, ToolKind::ReadOnly)
        | (PermissionMode::Auto, _)
        | (PermissionMode::AcceptEdits, ToolKind::Edit) => PermissionDecision::Allow,
        (PermissionMode::Plan, _) => PermissionDecision::Deny,
        (PermissionMode::AcceptEdits, ToolKind::Exec) | (PermissionMode::Ask, _) => {
            PermissionDecision::Ask
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_policy_matrix() {
        // Read-only is always allowed, regardless of mode.
        assert_eq!(
            evaluate(PermissionMode::Ask, "read_file"),
            PermissionDecision::Allow
        );
        assert_eq!(
            evaluate(PermissionMode::Plan, "graph_query"),
            PermissionDecision::Allow
        );
        // Auto allows everything.
        assert_eq!(
            evaluate(PermissionMode::Auto, "bash"),
            PermissionDecision::Allow
        );
        assert_eq!(
            evaluate(PermissionMode::Auto, "write_file"),
            PermissionDecision::Allow
        );
        // Plan denies every mutation (read-only mode).
        assert_eq!(
            evaluate(PermissionMode::Plan, "write_file"),
            PermissionDecision::Deny
        );
        assert_eq!(
            evaluate(PermissionMode::Plan, "exec_bash"),
            PermissionDecision::Deny
        );
        // Ask asks on every mutation.
        assert_eq!(
            evaluate(PermissionMode::Ask, "write_file"),
            PermissionDecision::Ask
        );
        assert_eq!(
            evaluate(PermissionMode::Ask, "bash"),
            PermissionDecision::Ask
        );
        // AcceptEdits: known edits auto-allow; shell + unknown still ask.
        assert_eq!(
            evaluate(PermissionMode::AcceptEdits, "edit_file"),
            PermissionDecision::Allow
        );
        assert_eq!(
            evaluate(PermissionMode::AcceptEdits, "write_file"),
            PermissionDecision::Allow
        );
        assert_eq!(
            evaluate(PermissionMode::AcceptEdits, "bash"),
            PermissionDecision::Ask
        );
        assert_eq!(
            evaluate(PermissionMode::AcceptEdits, "mystery_tool"),
            PermissionDecision::Ask
        );
    }

    #[test]
    fn permission_mode_roundtrip_and_cycle() {
        for m in [
            PermissionMode::Ask,
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
            PermissionMode::Auto,
        ] {
            assert_eq!(PermissionMode::parse_lenient(m.as_str()), m);
        }
        assert_eq!(
            PermissionMode::parse_lenient("garbage"),
            PermissionMode::Ask
        );
        assert_eq!(
            PermissionMode::parse_lenient("accept-edits"),
            PermissionMode::AcceptEdits
        );
        // Full Shift+Tab cycle returns to start.
        assert_eq!(
            PermissionMode::Ask.next().next().next().next(),
            PermissionMode::Ask
        );
    }
}
