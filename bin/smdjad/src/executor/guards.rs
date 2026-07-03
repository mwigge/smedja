//! Pre-dispatch guards enforced before any tool body runs: least-privilege
//! (read-only sessions), workspace path traversal, the `confirm_edits` hook, and
//! the methodology gate.
//!
//! [`reject_before_dispatch`] returns `Some(error)` to short-circuit
//! [`execute_tool`](super::execute_tool) (bypassing the output scan, matching the
//! pre-split behaviour), or `None` to proceed.

use serde_json::Value;
use smedja_ingot::{IngotHandle, Session};

use crate::executor::config::is_confirm_edits_enabled;
use crate::executor::fs_tools::{
    assert_within_workspace, extract_proposed_content, read_current_content,
};

/// Runs every pre-dispatch guard in order, returning the first rejection string
/// or `None` when the call may proceed to the tool body.
pub(crate) async fn reject_before_dispatch(
    tool_name: &str,
    input: &Value,
    workspace: &std::path::Path,
    session: Option<&Session>,
    ingot: &IngotHandle,
) -> Option<String> {
    // Least-privilege enforcement: block write tools for read-only (review) sessions.
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

    // Path traversal guard: reject write_file / edit_file paths outside workspace.
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

    // confirm_edits gate: when the workspace [tools] config has confirm_edits = true,
    // edit_file calls are flagged for cowork approval before writing. The full async
    // cowork approval gate is a roadmap item; the current release logs and proceeds so
    // that the config surface is live and the hook point is in place.
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

    // Methodology gate: block non-conforming writes for gated sessions. Runs
    // after the path-traversal guard and before any bytes are written (the actual
    // write is performed downstream — by an MCP file tool — only if we proceed).
    if matches!(tool_name, "write_file" | "edit_file") {
        if let Some(rejection) = methodology_gate(tool_name, input, workspace, session, ingot).await
        {
            return Some(rejection);
        }
    }

    None
}

/// The spec-first / diff-level methodology gate for `write_file` / `edit_file`.
async fn methodology_gate(
    tool_name: &str,
    input: &Value,
    workspace: &std::path::Path,
    session: Option<&Session>,
    ingot: &IngotHandle,
) -> Option<String> {
    let s = session?;
    let session_id = s.id.to_string();
    let state = ingot
        .get_methodology_state(&session_id)
        .await
        .unwrap_or_default();
    // The escape hatch bypasses both the spec-first check and diff gates.
    if state.no_spec_gate {
        return None;
    }
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
    None
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::executor::execute_tool;

    fn test_embedder() -> Arc<dyn crate::embedder_port::Embedder> {
        Arc::new(crate::embedder_port::FnvEmbedder::new())
    }

    fn session_with_mode(mode: Option<&str>) -> smedja_ingot::Session {
        smedja_ingot::Session {
            id: uuid::Uuid::new_v4(),
            created_at: smedja_types::Timestamp::from_micros(0),
            updated_at: smedja_types::Timestamp::from_micros(0),
            status: "active".to_owned(),
            task_id: None,
            mode: mode.map(str::to_owned),
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        }
    }

    async fn run_write(
        ingot: &smedja_ingot::IngotHandle,
        session: &smedja_ingot::Session,
        workspace: &std::path::Path,
        content: &str,
    ) -> String {
        use smedja_vault::Vault;
        use tokio::sync::Mutex;
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let input = serde_json::json!({ "path": "f.rs", "content": content }).to_string();
        execute_tool(
            "write_file",
            &input,
            workspace,
            Some(session),
            ingot,
            &vault,
            &test_embedder(),
        )
        .await
    }

    // ── path traversal guard ──────────────────────────────────────────────────

    #[tokio::test]
    async fn execute_tool_bash_returns_error_for_path_outside_workspace() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

        // write_file with a path that tries to escape the workspace via ../
        let result = execute_tool(
            "write_file",
            r#"{"path":"../../etc/passwd","content":"injected"}"#,
            std::path::Path::new("/tmp"),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;

        assert!(
            result.contains("path outside workspace") || result.starts_with("error:"),
            "path traversal must be rejected; got: {result}"
        );
    }

    // ── methodology gate ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn clean_mode_blocks_unwrap_write() {
        let ingot = smedja_ingot::IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap());
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let session = session_with_mode(Some("clean"));
        let result = run_write(&ingot, &session, &ws, "fn f() {\n    x.unwrap()\n}\n").await;
        assert!(result.contains("METHODOLOGY_BLOCKED"), "got: {result}");
        assert!(result.contains("CleanGate"), "got: {result}");
    }

    #[tokio::test]
    async fn clean_mode_allows_conforming_write() {
        let ingot = smedja_ingot::IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap());
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let session = session_with_mode(Some("clean"));
        // Clean content passes the gate; it then falls through to MCP dispatch.
        let result = run_write(&ingot, &session, &ws, "fn f() -> u32 {\n    1\n}\n").await;
        assert!(
            !result.contains("METHODOLOGY_BLOCKED"),
            "conforming write must pass the gate; got: {result}"
        );
    }

    #[tokio::test]
    async fn unmoded_session_is_ungated() {
        let ingot = smedja_ingot::IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap());
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let session = session_with_mode(None);
        let result = run_write(&ingot, &session, &ws, "fn f() {\n    x.unwrap()\n}\n").await;
        assert!(
            !result.contains("METHODOLOGY_BLOCKED"),
            "a session with no mode must not be gated; got: {result}"
        );
    }

    #[tokio::test]
    async fn no_spec_gate_escape_hatch_bypasses_gates() {
        let ingot = smedja_ingot::IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap());
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let session = session_with_mode(Some("clean"));
        ingot
            .set_no_spec_gate(&session.id.to_string(), true)
            .await
            .unwrap();
        let result = run_write(&ingot, &session, &ws, "fn f() {\n    x.unwrap()\n}\n").await;
        assert!(
            !result.contains("METHODOLOGY_BLOCKED"),
            "escape hatch must bypass the gate; got: {result}"
        );
    }

    #[tokio::test]
    async fn spec_mode_blocks_write_until_spec_and_approval_recorded() {
        let ingot = smedja_ingot::IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap());
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let session = session_with_mode(Some("spec"));

        // No spec recorded → blocked, naming the missing specification.
        let blocked = run_write(&ingot, &session, &ws, "fn f() -> u32 { 1 }\n").await;
        assert!(blocked.contains("METHODOLOGY_BLOCKED"), "got: {blocked}");
        assert!(blocked.contains("specification"), "got: {blocked}");

        // Record spec + approval → the spec-first gate releases.
        let sid = session.id.to_string();
        ingot.set_spec_recorded(&sid, true).await.unwrap();
        ingot.set_approval_recorded(&sid, true).await.unwrap();
        let released = run_write(&ingot, &session, &ws, "fn f() -> u32 { 1 }\n").await;
        assert!(
            !released.contains("METHODOLOGY_BLOCKED"),
            "spec+approval must release the spec-first gate; got: {released}"
        );
    }
}
