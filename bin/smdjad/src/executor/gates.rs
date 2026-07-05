//! Pre-dispatch auth / approval / methodology gates for `execute_tool`.
//!
//! Each guard returns `Some(error_string)` when the call must be rejected — the
//! dispatcher returns that verbatim (bypassing the output scan), reproducing the
//! original inline `return`s. `None` means the guard passed. `confirm_edits_gate`
//! only logs and has no reject path.

use serde_json::Value;
use smedja_ingot::{IngotHandle, Session};

use super::fs_tools::{assert_within_workspace, extract_proposed_content, read_current_content};

/// Least-privilege enforcement: block write tools for read-only (review) sessions.
pub(crate) fn review_write_block(tool_name: &str, session: Option<&Session>) -> Option<String> {
    if session.is_some_and(|s| s.mode.as_deref() == Some("review")) {
        const WRITE_TOOLS: &[&str] = &[
            "edit_file",
            "bash",
            "write_file",
            "run_command",
            "move_file",
            "copy_file",
            "delete_file",
        ];
        if WRITE_TOOLS.contains(&tool_name) {
            tracing::warn!(
                tool = tool_name,
                "smedja.security.tool_blocked: write tool blocked for read-only session"
            );
            return Some(format!(
                "error: tool '{tool_name}' is blocked for read-only roles (TOOL_BLOCKED)"
            ));
        }
    }
    None
}

/// Path traversal guard: reject `write_file` / `edit_file` paths outside workspace.
pub(crate) fn path_traversal_guard(
    tool_name: &str,
    input: &Value,
    workspace: &std::path::Path,
) -> Option<String> {
    if matches!(tool_name, "write_file" | "edit_file") {
        if let Some(path_str) = input.get("path").and_then(Value::as_str) {
            if let Err(err) = assert_within_workspace(workspace, path_str) {
                tracing::warn!(
                    tool = tool_name,
                    path = path_str,
                    "smedja.security.data_access_blocked: write outside workspace rejected"
                );
                return Some(err);
            }
        }
    }
    None
}

/// confirm_edits gate: when the workspace `[tools]` config has `confirm_edits = true`,
/// `edit_file` calls are flagged for cowork approval before writing. The full async
/// cowork approval gate is a roadmap item; the current release logs and proceeds so
/// that the config surface is live and the hook point is in place.
pub(crate) fn confirm_edits_gate(tool_name: &str, input: &Value, workspace: &std::path::Path) {
    if tool_name == "edit_file" {
        if let Some(path_str) = input.get("path").and_then(Value::as_str) {
            if is_confirm_edits_enabled(workspace) {
                tracing::info!(
                    path = path_str,
                    "confirm_edits: edit_file proceeding (full cowork gate is in roadmap)"
                );
            }
        }
    }
}

/// Methodology gate: block non-conforming writes for gated sessions. Runs
/// after the path-traversal guard and before any bytes are written (the actual
/// write is performed downstream — by an MCP file tool — only if we proceed).
pub(crate) async fn methodology_gate(
    tool_name: &str,
    input: &Value,
    workspace: &std::path::Path,
    session: Option<&Session>,
    ingot: &IngotHandle,
) -> Option<String> {
    if matches!(tool_name, "write_file" | "edit_file") {
        if let Some(s) = session {
            let session_id = s.id.to_string();
            let state = ingot
                .get_methodology_state(&session_id)
                .await
                .unwrap_or_default();
            // The escape hatch bypasses both the spec-first check and diff gates.
            if !state.no_spec_gate {
                let mode = crate::methodology_gate::parse_mode(s.mode.as_deref());
                if matches!(mode, Some(smedja_methodology::Mode::Spec)) {
                    // Spec-first lifecycle: no writes until spec and approval recorded.
                    if !(state.spec_recorded && state.approval_recorded) {
                        let missing = if state.spec_recorded {
                            "approval"
                        } else {
                            "specification"
                        };
                        tracing::warn!(
                            tool = tool_name,
                            session = %session_id,
                            "smedja.methodology.blocked: spec-first gate blocked write"
                        );
                        return Some(format!(
                            "error: spec-first gate — record a {missing} for the active task \
                             before writing files (METHODOLOGY_BLOCKED)"
                        ));
                    }
                } else if let Some(mode) = mode {
                    // Diff-level gate for tdd / clean / ponytail modes.
                    if let Some(proposed) = extract_proposed_content(input) {
                        let path_str = input
                            .get("path")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let current = read_current_content(workspace, path_str).await;
                        let diff = crate::methodology_gate::build_added_diff(&current, &proposed);
                        if let Some(violation) = crate::methodology_gate::run_gates(&mode, &diff) {
                            tracing::warn!(
                                tool = tool_name,
                                gate = violation.gate,
                                "smedja.methodology.blocked: gate blocked write"
                            );
                            return Some(format!(
                                "error: {} — {} (METHODOLOGY_BLOCKED)",
                                violation.gate, violation.message
                            ));
                        }
                    }
                }
            }
        }
    }
    None
}

/// Returns `true` when the workspace `[tools]` config has `confirm_edits = true`.
///
/// Reads `<workspace>/.smedja/workspace.toml`.  A missing or unparseable file
/// resolves to `false` so startup is never blocked by config trouble.
pub(crate) fn is_confirm_edits_enabled(workspace: &std::path::Path) -> bool {
    #[derive(serde::Deserialize, Default)]
    struct WorkspaceToml {
        tools: Option<ToolsSection>,
    }
    #[derive(serde::Deserialize, Default)]
    struct ToolsSection {
        confirm_edits: Option<bool>,
    }
    let path = workspace.join(".smedja").join("workspace.toml");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str::<WorkspaceToml>(&s).ok())
        .and_then(|c| c.tools?.confirm_edits)
        .unwrap_or(false)
}
