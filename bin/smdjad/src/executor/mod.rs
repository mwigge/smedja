//! Tool dispatch layer for the smedja agent daemon.
//!
//! This module owns the [`execute_tool`] entry point plus the pre-dispatch
//! security/methodology guards and the advisory output scan. Each tool's body
//! lives in a focused sibling submodule (`bash_tool`, `file_edit`, `file_search`,
//! `file_ops`, `vault_tools`, `sre_tools`, `misc_tools`, `web`); shared helpers
//! live in `fs_tools`, `config`, `output_filter`, `parse`, and `mcp_dispatch`.
//!
//! `exec_bash_ext` lives in `main.rs` and is re-used via `crate::exec_bash_ext`
//! because it has additional callers in the supervision tree.

use std::sync::Arc;

use serde_json::Value;
use smedja_ingot::{IngotHandle, Session};
use smedja_vault::Vault;
use tokio::sync::Mutex;

pub(crate) mod bash_tool;
pub(crate) mod config;
pub(crate) mod file_edit;
pub(crate) mod file_ops;
pub(crate) mod file_search;
pub(crate) mod fs_tools;
pub(crate) mod glob;
pub(crate) mod guards;
pub(crate) mod mcp_dispatch;
pub(crate) mod misc_tools;
pub(crate) mod output_filter;
pub(crate) mod parse;
pub(crate) mod security_scan;
pub(crate) mod sre_tools;
pub(crate) mod vault_tools;
pub(crate) mod web;

use fs_tools::assert_within_workspace;
use mcp_dispatch::dispatch_mcp_tool;
use security_scan::scan_tool_output;

// Re-exports preserving the pre-split `crate::executor::*` surface used elsewhere
// in the crate.
pub(crate) use mcp_dispatch::resolve_mcp_token;
pub(crate) use parse::{parse_all_tool_calls, parse_tool_call};

/// Resolves a caller-supplied report path against `workspace`, asserting it
/// stays within the workspace root.
///
/// Used by the auditor to write its markdown report through the same
/// boundary check the write tools enforce.
///
/// # Errors
///
/// Returns the workspace-boundary error string when the path escapes `workspace`.
pub(crate) fn audit_report_path(
    workspace: &std::path::Path,
    path_str: &str,
) -> Result<std::path::PathBuf, String> {
    assert_within_workspace(workspace, path_str)
}

/// Resolves the sandbox confined root for a tool execution.
///
/// `workspace` is the resolved task workspace — the active worktree path when a
/// task owns one, otherwise the session workspace (the orchestrator threads the
/// worktree path through as `workspace_root`). The root is canonicalised using
/// the same contract as [`assert_within_workspace`] (`.` against the workspace
/// itself), so the kernel boundary is rooted exactly where the path checks are.
pub(crate) fn confined_root_for(workspace: &std::path::Path) -> std::path::PathBuf {
    assert_within_workspace(workspace, ".").unwrap_or_else(|_| workspace.to_owned())
}

/// Test accessor for the read-only bash gate, exposing the `fs_tools` predicate
/// to sibling modules' tests.
#[cfg(test)]
#[must_use]
pub(crate) fn role_allows_write_bash_for_test(session: &smedja_ingot::Session) -> bool {
    fs_tools::role_allows_write_bash(session)
}

/// Local-tool allowlist shared with the `OTel` classification logic in `run_turn`.
///
/// Every tool whose dispatch is handled natively inside [`execute_tool`] must
/// appear here.  Anything absent from this list is routed to [`dispatch_mcp_tool`]
/// and classified as an `"extension"` in telemetry.
pub(crate) const LOCAL_TOOLS: &[&str] = &[
    "bash",
    "run_command",
    "read_file",
    "write_file",
    "edit_file",
    "list_files",
    "grep_files",
    "find_files",
    "move_file",
    "copy_file",
    "delete_file",
    "fetch_web",
    "smedja_vault_search",
    "smedja_vault_store",
    "smedja_retrieve",
    "graph_query",
    "load_skill",
    "otel_query",
    "metric_query",
    "log_tail",
];

/// Read-safe subset of [`LOCAL_TOOLS`] exposed by MCP server mode.
///
/// These tools cannot mutate the workspace or run shell commands, so they are
/// safe to share with arbitrary external MCP clients.  The mutating/exec tools
/// (`write_file`, `edit_file`, `bash`, `run_command`) and `smedja_vault_store`
/// are deliberately excluded; `tools/call` additionally routes through
/// [`execute_tool`] under an effective `review`-mode session, so the
/// `WRITE_TOOLS` guard rejects mutating tools even if this list drifts.
pub(crate) const MCP_SERVER_TOOLS: &[&str] = &[
    "graph_query",
    "grep_files",
    "find_files",
    "read_file",
    "list_files",
    "load_skill",
    "smedja_vault_search",
    "smedja_retrieve",
    "otel_query",
    "metric_query",
    "log_tail",
];

/// Read-only tools that can run concurrently without cowork gate approval.
///
/// These tools never mutate workspace state, so they are safe to dispatch in
/// parallel. Write/exec tools are not in this set and must be gated serially.
pub(crate) const READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "list_files",
    "grep_files",
    "find_files",
    "graph_query",
    "load_skill",
    "otel_query",
    "metric_query",
    "log_tail",
    "smedja_vault_search",
    "smedja_retrieve",
];

/// Vault namespace under which full uncompressed command output is teed for
/// recovery via the `smedja_retrieve` tool.
pub(crate) const FILTER_RECOVERY_NAMESPACE: &str = "filter-recovery";

/// Byte length above which a tool response is offloaded to a temp file rather
/// than injected verbatim into the agent context. Keeps large reads/fetches
/// from saturating the context window.
pub(crate) const LARGE_RESPONSE_THRESHOLD: usize = 100_000;

/// Executes the named tool with the given JSON input string.
///
/// Supported tools: `bash`, `run_command`, `read_file`, `list_files`, vault tools,
/// graph tools, SRE tools.  Unknown tools are forwarded to [`dispatch_mcp_tool`].
pub(crate) async fn execute_tool(
    tool_name: &str,
    tool_input: &str,
    workspace: &std::path::Path,
    session: Option<&Session>,
    ingot: &IngotHandle,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn crate::embedder_port::Embedder>,
) -> String {
    let input: Value = serde_json::from_str(tool_input).unwrap_or(Value::Null);

    // Pre-dispatch guards (least-privilege, path traversal, confirm_edits,
    // methodology). A rejection short-circuits before the tool body runs and
    // before the output scan, matching the pre-split behaviour.
    if let Some(rejection) =
        guards::reject_before_dispatch(tool_name, &input, workspace, session, ingot).await
    {
        return rejection;
    }

    let result = match tool_name {
        "bash" | "run_command" => {
            bash_tool::run_bash(tool_name, &input, workspace, session, ingot, vault).await
        }
        "read_file" => file_edit::read_file(&input, workspace).await,
        "write_file" => file_edit::write_file(&input, workspace).await,
        "edit_file" => file_edit::edit_file(&input, workspace).await,
        "list_files" => file_search::list_files(&input, workspace).await,
        "grep_files" => file_search::grep_files(&input, workspace).await,
        "find_files" => file_search::find_files(&input, workspace).await,
        "move_file" => file_ops::move_file(&input, workspace),
        "copy_file" => file_ops::copy_file(&input, workspace),
        "delete_file" => file_ops::delete_file(&input, workspace),
        "smedja_vault_search" => vault_tools::vault_search(&input, vault, embedder).await,
        "smedja_vault_store" => vault_tools::vault_store(&input, vault, embedder).await,
        "smedja_retrieve" => vault_tools::retrieve(&input).await,
        "load_skill" => misc_tools::load_skill(&input),
        "graph_query" => misc_tools::graph_query(&input, workspace),
        "alert_list" => misc_tools::alert_list().await,
        "otel_query" => sre_tools::otel_query(&input).await,
        "metric_query" => sre_tools::metric_query(&input).await,
        "log_tail" => sre_tools::log_tail(&input).await,
        "fetch_web" => web::fetch_web(&input).await,
        other => dispatch_mcp_tool(other, &input, ingot).await,
    };

    // Advisory output scanning on the tool-result return path. A high-signal
    // secret match records a `security_finding` audit event; by default
    // (enforcement off) the content is returned unmodified.
    scan_tool_output(&result, tool_name, workspace, session, ingot).await
}

#[cfg(test)]
mod tests {
    // ── MCP_SERVER_TOOLS subset ───────────────────────────────────────────────

    #[test]
    fn mcp_server_tools_are_exactly_the_read_safe_subset() {
        // No write/exec tool may appear in the server subset.
        const FORBIDDEN: &[&str] = &[
            "edit_file",
            "bash",
            "write_file",
            "run_command",
            "move_file",
            "copy_file",
            "delete_file",
            "smedja_vault_store",
        ];
        let expected = [
            "graph_query",
            "grep_files",
            "find_files",
            "read_file",
            "list_files",
            "load_skill",
            "smedja_vault_search",
            "smedja_retrieve",
            "otel_query",
            "metric_query",
            "log_tail",
        ];
        // The exposed subset must match the read-safe list exactly.
        let mut got = super::MCP_SERVER_TOOLS.to_vec();
        let mut want = expected.to_vec();
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want, "MCP_SERVER_TOOLS must be the read-safe subset");

        for tool in FORBIDDEN {
            assert!(
                !super::MCP_SERVER_TOOLS.contains(tool),
                "MCP_SERVER_TOOLS must not expose write/exec tool '{tool}'"
            );
        }
    }

    #[test]
    fn mcp_server_tools_is_subset_of_local_tools() {
        for tool in super::MCP_SERVER_TOOLS {
            assert!(
                super::LOCAL_TOOLS.contains(tool),
                "MCP_SERVER_TOOLS entry '{tool}' must also be in LOCAL_TOOLS"
            );
        }
    }

    // ── confined root resolution (worktree-aware) ─────────────────────────────

    #[test]
    fn confined_root_is_worktree_when_task_owns_one() {
        // The orchestrator threads the active worktree path through as the
        // tool-execution workspace when a task owns one, else the session
        // workspace. `confined_root_for` must canonicalise that subtree.
        let session_ws = tempfile::tempdir().unwrap();
        let worktree = session_ws.path().join("worktrees").join("task-1");
        std::fs::create_dir_all(&worktree).unwrap();

        // Task owns a worktree → confined root is the worktree.
        let resolved = super::confined_root_for(&worktree);
        assert_eq!(resolved, worktree.canonicalize().unwrap());

        // No worktree → confined root is the session workspace.
        let resolved = super::confined_root_for(session_ws.path());
        assert_eq!(resolved, session_ws.path().canonicalize().unwrap());
    }
}
