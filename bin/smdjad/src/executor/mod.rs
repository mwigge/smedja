//! Tool dispatch layer for the smedja agent daemon.
//!
//! This module owns the [`execute_tool`] entry point plus its direct helpers:
//! [`find_tool_call_json`], [`parse_tool_call`], and [`dispatch_mcp_tool`].
//! Filesystem-path helpers (workspace-boundary checks, content reads, role gating)
//! live in the [`fs_tools`] submodule.
//!
//! `exec_bash` lives in `main.rs` and is re-used via `super::exec_bash` because it
//! has additional callers in the supervision tree.

use std::sync::Arc;

use serde_json::Value;
use smedja_ingot::{IngotHandle, Session};
use smedja_vault::Vault;
use tokio::sync::Mutex;

pub(crate) mod fs_tools;
use fs_tools::assert_within_workspace;

mod gates;
#[cfg(test)]
use gates::is_confirm_edits_enabled;

mod tool_parse;
#[cfg(test)]
use tool_parse::find_tool_call_json;
pub(crate) use tool_parse::{parse_all_tool_calls, parse_tool_call};

mod output_filter;
pub(crate) use output_filter::{filter_command_output, retrieve_store};
// Re-exported for the intra-doc link in `crate::lean_spec`; not referenced in code.
#[allow(unused_imports)]
pub(crate) use output_filter::FILTER_RECOVERY_NAMESPACE;
#[cfg(test)]
use output_filter::{content_hash, LARGE_RESPONSE_THRESHOLD};

mod handlers;
#[cfg(test)]
use handlers::fs::{bash_config, glob_match};
#[cfg(test)]
use handlers::web::strip_html;

mod mcp_dispatch;
#[cfg(test)]
use mcp_dispatch::dispatch_mcp_tool_with_store;
pub(crate) use mcp_dispatch::{dispatch_mcp_tool, resolve_mcp_token};

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
/// Load and return a named skill from `skills_dir`, wrapped in an XML envelope.
pub(crate) fn execute_load_skill(name: &str, skills_dir: &std::path::Path) -> String {
    let registry = smedja_plugins::SkillRegistry::new(skills_dir);
    match registry.find(name) {
        Ok(Some(skill)) => smedja_plugins::wrap_skill_body(&skill.manifest.name, &skill.body),
        Ok(None) => format!(
            "error: skill '{name}' not found in {}",
            skills_dir.display()
        ),
        Err(e) => format!("error: skill registry error: {e}"),
    }
}

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
    "lsp_definition",
    "lsp_references",
    "lsp_hover",
    "lsp_document_symbols",
    "lsp_workspace_symbols",
    "lsp_rename_symbol",
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
    "lsp_definition",
    "lsp_references",
    "lsp_hover",
    "lsp_document_symbols",
    "lsp_workspace_symbols",
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
    "lsp_definition",
    "lsp_references",
    "lsp_hover",
    "lsp_document_symbols",
    "lsp_workspace_symbols",
];

/// Executes the named tool with the given JSON input string.
///
/// Supported tools: `bash`, `run_command`, `read_file`, `list_files`, vault tools,
/// graph tools, SRE tools.  Unknown tools are forwarded to [`dispatch_mcp_tool`].
#[allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::too_many_arguments
)]
pub(crate) async fn execute_tool(
    tool_name: &str,
    tool_input: &str,
    workspace: &std::path::Path,
    session: Option<&Session>,
    ingot: &IngotHandle,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn crate::embedder_port::Embedder>,
    lsp: Option<&Arc<smedja_lsp::LspManager>>,
) -> String {
    let input: Value = serde_json::from_str(tool_input).unwrap_or(Value::Null);

    // Pre-dispatch gates. Each returns the rejection string to surface verbatim
    // (bypassing the output scan), exactly as the original inline `return`s did.
    if let Some(err) = gates::review_write_block(tool_name, session) {
        return err;
    }
    if let Some(err) = gates::path_traversal_guard(tool_name, &input, workspace) {
        return err;
    }
    gates::confirm_edits_gate(tool_name, &input, workspace);
    if let Some(err) = gates::methodology_gate(tool_name, &input, workspace, session, ingot).await {
        return err;
    }

    // Native tool bodies delegate to the `handlers` submodules; `execute_tool`
    // is the dispatcher. Handlers that guard input with early returns yield
    // `Err`, which is returned verbatim (bypassing the output scan) to preserve
    // the original arms' `return` control flow; `Ok` bodies flow through the
    // scan below exactly as the arm tail values did.
    let result = match tool_name {
        "bash" | "run_command" => {
            match handlers::fs::bash(tool_name, &input, workspace, session, ingot, vault).await {
                Ok(r) => r,
                Err(e) => return e,
            }
        }
        "read_file" => match handlers::fs::read_file(&input, workspace).await {
            Ok(r) => r,
            Err(e) => return e,
        },
        "list_files" => match handlers::fs::list_files(&input, workspace).await {
            Ok(r) => r,
            Err(e) => return e,
        },
        "grep_files" => match handlers::fs::grep_files(&input, workspace).await {
            Ok(r) => r,
            Err(e) => return e,
        },
        "find_files" => match handlers::fs::find_files(&input, workspace).await {
            Ok(r) => r,
            Err(e) => return e,
        },
        "move_file" => match handlers::fs::move_file(&input, workspace).await {
            Ok(r) => r,
            Err(e) => return e,
        },
        "copy_file" => match handlers::fs::copy_file(&input, workspace).await {
            Ok(r) => r,
            Err(e) => return e,
        },
        "delete_file" => match handlers::fs::delete_file(&input, workspace).await {
            Ok(r) => r,
            Err(e) => return e,
        },
        "smedja_vault_search" => handlers::vault::vault_search(&input, vault, embedder).await,
        "smedja_vault_store" => handlers::vault::vault_store(&input, vault, embedder).await,
        "smedja_retrieve" => handlers::vault::retrieve(&input).await,
        "load_skill" => {
            let name = input.get("name").and_then(Value::as_str).unwrap_or("");
            execute_load_skill(name, &smedja_plugins::SkillRegistry::default_path())
        }
        "graph_query" => match handlers::graph::graph_query(&input, workspace).await {
            Ok(r) => r,
            Err(e) => return e,
        },
        "alert_list" => {
            let alerts = crate::alert::drain_alerts(50).await;
            serde_json::to_string(&alerts).unwrap_or_default()
        }
        "otel_query" => match handlers::sre::otel_query(&input).await {
            Ok(r) => r,
            Err(e) => return e,
        },
        "metric_query" => match handlers::sre::metric_query(&input).await {
            Ok(r) => r,
            Err(e) => return e,
        },
        "log_tail" => match handlers::sre::log_tail(&input).await {
            Ok(r) => r,
            Err(e) => return e,
        },
        "fetch_web" => match handlers::web::fetch_web(&input).await {
            Ok(r) => r,
            Err(e) => return e,
        },
        "lsp_definition"
        | "lsp_references"
        | "lsp_hover"
        | "lsp_document_symbols"
        | "lsp_workspace_symbols"
        | "lsp_rename_symbol" => match lsp {
            Some(mgr) => handlers::lsp::dispatch(tool_name, &input, mgr, workspace).await,
            None => "error: LSP tools are unavailable in this context (no language-server manager)"
                .to_owned(),
        },
        other => dispatch_mcp_tool(other, &input, ingot).await,
    };

    // Advisory output scanning on the tool-result return path. A high-signal
    // secret match records a `security_finding` audit event; by default
    // (enforcement off) the content is returned unmodified.
    scan_tool_output(&result, tool_name, workspace, session, ingot).await
}

/// Scans a tool result for secret patterns and records any match as an advisory
/// `security_finding` audit event, returning the content to surface to the
/// caller.
///
/// Advisory by default: with enforcement off (the default config) the original
/// `result` is returned unmodified and findings carry `status = "warn"`. When
/// the `[security]` config enforces at or above a match's severity, the matched
/// span is redacted and the finding carries `status = "blocked"`.
async fn scan_tool_output(
    result: &str,
    tool_name: &str,
    workspace: &std::path::Path,
    session: Option<&Session>,
    ingot: &IngotHandle,
) -> String {
    let config = crate::security::load_security_config(workspace);
    let scan = smedja_security::scan_output(result, &config);
    if scan.is_clean() {
        return scan.content;
    }

    let session_id = session.map_or_else(|| "smdjad".to_owned(), |s| s.id.to_string());
    for finding in &scan.findings {
        tracing::warn!(
            tool = tool_name,
            rule = %finding.rule_id,
            severity = %finding.severity.as_str(),
            status = %finding.status_for(&config),
            "smedja.security.output_finding"
        );
        let mut event = finding.to_audit_event(&session_id, &config);
        event.tool_name = Some(tool_name.to_owned());
        if let Err(e) = ingot.insert_audit_event(event).await {
            tracing::warn!(error = %e, "failed to record output-scan finding; continuing");
        }
    }
    scan.content
}

#[cfg(test)]
mod tests;
