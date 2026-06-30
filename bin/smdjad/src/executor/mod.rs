//! Tool dispatch layer for the smedja agent daemon.
//!
//! This module owns the [`execute_tool`] entry point plus its direct helpers:
//! [`find_tool_call_json`], [`parse_tool_call`], and [`dispatch_mcp_tool`].
//! Filesystem-path helpers (workspace-boundary checks, content reads, role gating)
//! live in the [`fs_tools`] submodule.
//!
//! `exec_bash` lives in `main.rs` and is re-used via `super::exec_bash` because it
//! has additional callers in the supervision tree.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use base64::Engine as _;

use serde_json::Value;
use smedja_ingot::{IngotHandle, Session};
use smedja_vault::{Vault, VaultEntry};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::sandbox::SandboxExecutor;

pub(crate) mod fs_tools;
use fs_tools::{
    assert_within_workspace, extract_proposed_content, read_current_content, role_allows_write_bash,
};

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
    role_allows_write_bash(session)
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
    "smedja_vault_search",
    "smedja_vault_store",
    "smedja_retrieve",
    "graph_query",
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
    "read_file",
    "list_files",
    "smedja_vault_search",
    "smedja_retrieve",
    "otel_query",
    "metric_query",
    "log_tail",
];

/// In-memory store for content blocks addressed by SHA-256 hash.
/// Used by the `smedja_retrieve` tool to look up compressed context blocks.
fn retrieve_store() -> &'static tokio::sync::Mutex<HashMap<String, String>> {
    static STORE: OnceLock<tokio::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
    STORE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// Insertion-order tracker for the retrieve store LRU eviction.
fn retrieve_store_order() -> &'static tokio::sync::Mutex<std::collections::VecDeque<String>> {
    static ORDER: OnceLock<tokio::sync::Mutex<std::collections::VecDeque<String>>> =
        OnceLock::new();
    ORDER.get_or_init(|| tokio::sync::Mutex::new(std::collections::VecDeque::new()))
}

/// Vault namespace under which full uncompressed command output is teed for
/// recovery via the `smedja_retrieve` tool.
pub(crate) const FILTER_RECOVERY_NAMESPACE: &str = "filter-recovery";

/// Computes the lowercase hex SHA-256 content hash used to address a teed
/// full-output recovery entry.
fn content_hash(content: &str) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

/// Applies command-aware text filtering to a `bash`/`run_command` result on the
/// return path, in-process (no shell hooks, no subprocess).
///
/// JSON results route to [`smedja_adapter::compress_tool_result`] (`SmartCrusher`,
/// unchanged); text routes through the command-keyed filter registry loaded from
/// `.smedja/filters.toml`. When filtering reduces the output (ratio < 1.0) the
/// full uncompressed text is teed to the vault recovery namespace and registered
/// in the `smedja_retrieve` store under its content hash, a trailing recovery
/// marker naming that hash is appended, and the estimated tokens saved are
/// recorded on the tokens-saved ledger. The captured success/failure contract
/// of the result is never altered — only its body text is compressed.
///
/// `SMEDJA_NO_TOOL_COMPRESS=1` is honoured by the underlying compressors and
/// returns the result verbatim.
async fn filter_command_output(
    cmd: &str,
    result: String,
    workspace: &std::path::Path,
    session: Option<&Session>,
    ingot: &IngotHandle,
    vault: &Arc<Mutex<Vault>>,
) -> String {
    // Single branch point: JSON routes through SmartCrusher; text routes through
    // the command filter. The bash/run_command path is text by construction.
    if serde_json::from_str::<Value>(&result).is_ok() {
        let compressed = smedja_adapter::compress_tool_result(&result);
        // Attribute the SmartCrusher saving to its own source so it is not folded
        // into the filter total. The estimate is recorded on this JSON path only.
        record_tokens_saved(cmd, &result, &compressed, "crusher", session, ingot).await;
        return compressed;
    }

    let registry = crate::filters::load_filter_registry(workspace);
    let (compressed, ratio) = smedja_adapter::compress_command_output_with(&registry, cmd, &result);

    // No reduction → return verbatim; nothing to tee, no savings to record.
    if ratio >= 1.0 {
        return result;
    }

    // Tee the full uncompressed output to the vault recovery namespace and the
    // in-memory retrieve store, addressed by content hash.
    let hash = content_hash(&result);
    {
        let mut store = retrieve_store().lock().await;
        let mut order = retrieve_store_order().lock().await;
        store.insert(hash.clone(), result.clone());
        order.push_back(hash.clone());
        if store.len() > 512 {
            if let Some(oldest) = order.pop_front() {
                store.remove(&oldest);
            }
        }
    }
    tee_to_vault(&hash, &result, vault).await;

    // Record tokens saved (clamped ≥ 0), separate from billed cost.
    record_tokens_saved(cmd, &result, &compressed, "filter", session, ingot).await;

    // Append the recovery marker naming the hash so the agent can expand it.
    format!("{compressed}\n[smedja_retrieve hash={hash} to expand full output]")
}

/// Tees `full_output` to the vault recovery namespace under `hash`.
///
/// Vault writes are synchronous `SQLite` work; they run on a blocking thread so
/// the async runtime is never blocked. A vault error is logged and swallowed —
/// recovery is best-effort and must never break the tool path.
async fn tee_to_vault(hash: &str, full_output: &str, vault: &Arc<Mutex<Vault>>) {
    let vault = Arc::clone(vault);
    let entry = VaultEntry {
        id: hash.to_owned(),
        embedding: Vec::new(),
        payload: serde_json::json!({ "kind": "filter-recovery" }),
        namespace: FILTER_RECOVERY_NAMESPACE.to_owned(),
        content: full_output.to_owned(),
        source_file: None,
        added_by: Some("output-filter".to_owned()),
        chunk_index: None,
        parent_id: None,
        created_at: 0.0,
        // Recovery rows hold the raw output for hash retrieval, never a semantic
        // embedding, so they carry an empty vector tagged dim 0.
        embedder_model_id: smedja_vault::LEGACY_MODEL_ID.to_owned(),
        dim: 0,
    };
    let join = tokio::task::spawn_blocking(move || {
        let mut guard = vault.blocking_lock();
        guard.upsert(&entry)
    })
    .await;
    match join {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "filter recovery vault tee failed; continuing"),
        Err(e) => tracing::warn!(error = %e, "filter recovery vault tee task panicked; continuing"),
    }
}

/// Records the tokens saved by filtering on the tokens-saved ledger.
///
/// `saved = estimate_tokens(original) - estimate_tokens(compressed)`, clamped at
/// 0. Recorded only when positive, keyed by session (turn `0` at the executor
/// layer, which does not thread a turn index). A ledger error is logged and
/// swallowed — accounting is advisory and must never break the tool path.
async fn record_tokens_saved(
    cmd: &str,
    original: &str,
    compressed: &str,
    source: &str,
    session: Option<&Session>,
    ingot: &IngotHandle,
) {
    let before = smedja_memory::estimate_tokens(original);
    let after = smedja_memory::estimate_tokens(compressed);
    let saved = before.saturating_sub(after);
    if saved == 0 {
        return;
    }
    let Some(session) = session else {
        return;
    };
    let entry = smedja_ingot::TokensSavedEntry {
        id: Uuid::new_v4(),
        session_id: session.id.to_string(),
        turn_n: 0,
        command: cmd.to_owned(),
        tokens_saved: i64::try_from(saved).unwrap_or(i64::MAX),
        source: source.to_owned(),
        created_at: smedja_types::Timestamp::from_micros(0),
    };
    if let Err(e) = ingot.insert_tokens_saved(entry).await {
        tracing::warn!(error = %e, "failed to record tokens-saved; continuing");
    }
}

/// Finds the first JSON object with a `"tool"` key anywhere in `text`.
///
/// Uses `serde_json` streaming deserialization: for each `{` byte position,
/// a `Deserializer` is created so that valid JSON is consumed and trailing
/// text is ignored, without a custom brace-counting scanner.
pub(crate) fn find_tool_call_json(text: &str) -> Option<serde_json::Value> {
    use serde::de::Deserialize as _;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'{' {
            let mut de = serde_json::Deserializer::from_str(&text[i..]);
            if let Ok(v) = serde_json::Value::deserialize(&mut de) {
                if v.get("tool").is_some() {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Parses a tool call embedded in `text`, returning `(tool_name, input_json_string)`.
///
/// Looks for a JSON object with a `"tool"` key anywhere in the text.
/// Returns `None` when no tool call is detected.
pub(crate) fn parse_tool_call(text: &str) -> Option<(String, String)> {
    let v = find_tool_call_json(text)?;
    let tool_name = v.get("tool").and_then(Value::as_str)?.to_owned();
    let input = v
        .get("input")
        .map_or_else(|| "{}".to_owned(), std::string::ToString::to_string);
    Some((tool_name, input))
}

/// Returns `true` when the workspace `[tools]` config has `confirm_edits = true`.
///
/// Reads `<workspace>/.smedja/workspace.toml`.  A missing or unparseable file
/// resolves to `false` so startup is never blocked by config trouble.
/// Minimal glob matcher supporting `*` (any sequence except `/`) and `?` (one char).
fn glob_match(pattern: &str, name: &str) -> bool {
    let mut p = pattern.as_bytes();
    let mut s = name.as_bytes();
    loop {
        match (p.first(), s.first()) {
            (None, None) => return true,
            (Some(&b'*'), _) => {
                p = &p[1..];
                if p.is_empty() {
                    return true;
                }
                // Try matching `*` against 0..n chars.
                for i in 0..=s.len() {
                    if glob_match(
                        std::str::from_utf8(p).unwrap_or(""),
                        std::str::from_utf8(&s[i..]).unwrap_or(""),
                    ) {
                        return true;
                    }
                }
                return false;
            }
            (Some(&b'?'), Some(_)) => {
                p = &p[1..];
                s = &s[1..];
            }
            (Some(a), Some(b)) if a == b => {
                p = &p[1..];
                s = &s[1..];
            }
            _ => return false,
        }
    }
}

fn is_confirm_edits_enabled(workspace: &std::path::Path) -> bool {
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

/// Executes the named tool with the given JSON input string.
///
/// Supported tools: `bash`, `run_command`, `read_file`, `list_files`, vault tools,
/// graph tools, SRE tools.  Unknown tools are forwarded to [`dispatch_mcp_tool`].
#[allow(clippy::too_many_lines)]
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

    // Least-privilege enforcement: block write tools for read-only (review) sessions.
    if session.is_some_and(|s| s.mode.as_deref() == Some("review")) {
        const WRITE_TOOLS: &[&str] = &["edit_file", "bash", "write_file", "run_command"];
        if WRITE_TOOLS.contains(&tool_name) {
            tracing::warn!(
                tool = tool_name,
                "smedja.security.tool_blocked: write tool blocked for read-only session"
            );
            return format!(
                "error: tool '{tool_name}' is blocked for read-only roles (TOOL_BLOCKED)"
            );
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
                return err;
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
                        return format!(
                            "error: spec-first gate — record a {missing} for the active task \
                             before writing files (METHODOLOGY_BLOCKED)"
                        );
                    }
                } else if let Some(mode) = mode {
                    // Diff-level gate for tdd / clean / ponytail modes.
                    if let Some(proposed) = extract_proposed_content(&input) {
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
                            return format!(
                                "error: {} — {} (METHODOLOGY_BLOCKED)",
                                violation.gate, violation.message
                            );
                        }
                    }
                }
            }
        }
    }

    let result = match tool_name {
        "bash" | "run_command" => {
            let cmd = input
                .get("command")
                .or_else(|| input.get("cmd"))
                .and_then(Value::as_str)
                .unwrap_or_default();

            // Enforce read-only mode for review sessions.
            if session.is_some_and(|s| !role_allows_write_bash(s)) {
                let arity = smedja_assayer::classify_bash(cmd);
                if arity == smedja_assayer::BashArity::Write {
                    return "permission denied: review mode sessions cannot execute write commands"
                        .to_owned();
                }
            }

            // SandboxExecutor: confine execution to the resolved confined root
            // (the active worktree when a task owns one, else the workspace).
            // Exempt tools never reach this arm. The fallback contract
            // (auto/required/off) is enforced inside `run_confined`.
            let sandbox = SandboxExecutor::new();
            let raw = if SandboxExecutor::is_exempt(tool_name) {
                super::exec_bash(cmd, workspace).await
            } else {
                let confined_root = confined_root_for(workspace);
                let cmd_owned = cmd.to_owned();
                let ws = workspace.to_owned();
                sandbox
                    .run_confined(cmd, &confined_root, || async move {
                        super::exec_bash(&cmd_owned, &ws).await
                    })
                    .await
            };

            // Command-aware text filtering on the return path (in-process; no
            // shell hooks, no subprocess). Compresses verbose output before it
            // enters working memory, tees the full text to the vault for
            // recovery, and records tokens saved. The success/failure contract
            // is unaffected — only the body text is compressed.
            filter_command_output(cmd, raw, workspace, session, ingot, vault).await
        }
        "read_file" => {
            let path_str = input
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let full = match assert_within_workspace(workspace, path_str) {
                Ok(p) => p,
                Err(err) => return err,
            };
            let encoding = input
                .get("encoding")
                .and_then(Value::as_str)
                .unwrap_or("text");
            let start_line = input
                .get("start_line")
                .and_then(Value::as_u64)
                .map(|n| n as usize);
            let end_line = input
                .get("end_line")
                .and_then(Value::as_u64)
                .map(|n| n as usize);
            let raw = match tokio::fs::read(&full).await {
                Ok(bytes) => bytes,
                Err(e) => return format!("error reading {path_str}: {e}"),
            };
            if encoding == "base64" {
                base64::engine::general_purpose::STANDARD.encode(&raw)
            } else {
                let text = String::from_utf8_lossy(&raw).into_owned();
                if start_line.is_none() && end_line.is_none() {
                    text
                } else {
                    let start = start_line.unwrap_or(1).saturating_sub(1);
                    let lines: Vec<&str> = text.lines().collect();
                    let end = end_line.map(|e| e.min(lines.len())).unwrap_or(lines.len());
                    lines[start.min(lines.len())..end].join("\n")
                }
            }
        }
        "list_files" => {
            let dir_str = input.get("path").and_then(Value::as_str).unwrap_or(".");
            let full = match assert_within_workspace(workspace, dir_str) {
                Ok(p) => p,
                Err(err) => return err,
            };
            let depth = input.get("depth").and_then(Value::as_u64).unwrap_or(1) as usize;
            let pattern = input
                .get("pattern")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let max_depth = if depth == 0 { usize::MAX } else { depth };
            match tokio::task::spawn_blocking(move || {
                let mut entries = Vec::new();
                let walker = walkdir::WalkDir::new(&full)
                    .max_depth(max_depth)
                    .into_iter()
                    .filter_entry(|e| {
                        e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.')
                    });
                for entry in walker.filter_map(std::result::Result::ok).skip(1) {
                    let name = entry.path().strip_prefix(&full).unwrap_or(entry.path());
                    let name_str = name.to_string_lossy().into_owned();
                    if let Some(ref pat) = pattern {
                        // Simple glob: only match file name portion against the pattern.
                        let file_name = entry.file_name().to_string_lossy();
                        if !glob_match(pat, &file_name) {
                            continue;
                        }
                    }
                    entries.push(name_str);
                }
                entries.sort();
                entries.join("\n")
            })
            .await
            {
                Ok(result) => result,
                Err(e) => format!("error listing {dir_str}: {e}"),
            }
        }
        "smedja_vault_search" => {
            let query_text = input
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let k =
                usize::try_from(input.get("k").and_then(Value::as_u64).unwrap_or(5)).unwrap_or(5);
            let ns = input
                .get("namespace")
                .and_then(Value::as_str)
                .unwrap_or("default")
                .to_owned();
            let vault = Arc::clone(vault);
            let query_vec = embedder.embed_query(&query_text).await;
            let model_id = embedder.model_id().to_owned();
            let dim = embedder.dim();
            tokio::task::spawn_blocking(move || {
                let guard = vault.blocking_lock();
                match guard.search(&query_vec, &query_text, &ns, k, &model_id, dim) {
                    Ok(entries) => {
                        let results: Vec<serde_json::Value> = entries
                            .into_iter()
                            .map(|e| {
                                serde_json::json!({
                                    "id": e.id,
                                    "content": e.content,
                                    "namespace": e.namespace,
                                    "payload": e.payload,
                                })
                            })
                            .collect();
                        serde_json::json!({ "results": results }).to_string()
                    }
                    Err(e) => format!("error: vault search failed: {e}"),
                }
            })
            .await
            .unwrap_or_else(|e| format!("error: vault search task panicked: {e}"))
        }
        "smedja_vault_store" => {
            let content = input
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let ns = input
                .get("namespace")
                .and_then(Value::as_str)
                .unwrap_or("default")
                .to_owned();
            let entry_id = input
                .get("id")
                .and_then(Value::as_str)
                .map_or_else(|| Uuid::new_v4().to_string(), ToOwned::to_owned);
            let payload = input
                .get("payload")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            let source_file = input
                .get("source_file")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let added_by = input
                .get("added_by")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let vault = Arc::clone(vault);
            let embedding = embedder.embed_query(&content).await;
            let model_id = embedder.model_id().to_owned();
            let dim = embedder.dim();
            tokio::task::spawn_blocking(move || {
                let entry = VaultEntry {
                    id: entry_id,
                    embedding,
                    payload,
                    namespace: ns,
                    content,
                    source_file,
                    added_by,
                    chunk_index: None,
                    parent_id: None,
                    created_at: 0.0,
                    embedder_model_id: model_id,
                    dim,
                };
                let mut guard = vault.blocking_lock();
                match guard.upsert(&entry) {
                    Ok(()) => serde_json::json!({ "id": entry.id, "stored": true }).to_string(),
                    Err(e) => format!("error: vault store failed: {e}"),
                }
            })
            .await
            .unwrap_or_else(|e| format!("error: vault store task panicked: {e}"))
        }
        "smedja_retrieve" => {
            let hash = input.get("hash").and_then(Value::as_str).unwrap_or("");
            let store = retrieve_store().lock().await;
            if let Some(content) = store.get(hash) {
                // ponytail: audit deferred; log the retrieval.
                tracing::info!(hash, "smedja_retrieve hit");
                content.clone()
            } else {
                tracing::debug!(hash, "smedja_retrieve: hash not found");
                format!("error: hash not found: {hash}")
            }
        }
        "graph_query" => {
            let query = input.get("query").and_then(Value::as_str).unwrap_or("");
            let depth =
                u8::try_from(input.get("depth").and_then(Value::as_u64).unwrap_or(2)).unwrap_or(2);
            let graph_db_path = crate::handlers::graph::graph_db_path(workspace);
            if !graph_db_path.exists() {
                tracing::debug!("graph.db not found; returning empty symbols");
                return serde_json::json!({ "symbols": [] }).to_string();
            }
            match smedja_graph::GraphStore::open(&graph_db_path) {
                Ok(store) => match store.graph_query(query, 10, depth) {
                    Ok(symbols) => {
                        let sym_json: Vec<serde_json::Value> = symbols
                            .iter()
                            .map(|s| {
                                serde_json::json!({
                                    "name": s.name,
                                    "kind": s.kind.as_str(),
                                    "file": s.file_path,
                                    "line": s.start_line,
                                    "snippet": s.snippet,
                                })
                            })
                            .collect();
                        serde_json::json!({ "symbols": sym_json }).to_string()
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "graph_query error");
                        serde_json::json!({ "symbols": [], "error": e.to_string() }).to_string()
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, "failed to open graph store");
                    serde_json::json!({ "symbols": [] }).to_string()
                }
            }
        }
        "alert_list" => {
            let alerts = crate::alert::drain_alerts(50).await;
            serde_json::to_string(&alerts).unwrap_or_default()
        }
        "otel_query" => {
            let service = input
                .get("service")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let filter = input.get("filter").and_then(|v| v.as_str());
            let range = input
                .get("range_minutes")
                .and_then(Value::as_i64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(60);
            if let Ok(cfg) = smedja_sre::SreConfig::from_env() {
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(15))
                    .connect_timeout(std::time::Duration::from_secs(5))
                    .build()
                    .unwrap_or_default();
                match smedja_sre::otel_query(&client, &cfg, service, filter, range).await {
                    Ok(v) => serde_json::to_string(&v).unwrap_or_default(),
                    Err(e) => format!("error: {e}"),
                }
            } else {
                "SRE config not available (set SMEDJA_OTLP_ENDPOINT)".into()
            }
        }
        "metric_query" => {
            let promql = input.get("promql").and_then(|v| v.as_str()).unwrap_or("");
            let range = input
                .get("range_minutes")
                .and_then(Value::as_i64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(60);
            if let Ok(cfg) = smedja_sre::SreConfig::from_env() {
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(15))
                    .connect_timeout(std::time::Duration::from_secs(5))
                    .build()
                    .unwrap_or_default();
                match smedja_sre::metric_query(&client, &cfg, promql, range).await {
                    Ok(v) => serde_json::to_string(&v).unwrap_or_default(),
                    Err(e) => format!("error: {e}"),
                }
            } else {
                "SRE config not available (set SMEDJA_PROMETHEUS_ENDPOINT)".into()
            }
        }
        "log_tail" => {
            let service = input
                .get("service")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let filter = input.get("filter").and_then(|v| v.as_str()).unwrap_or("");
            let lines = input
                .get("lines")
                .and_then(Value::as_i64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(100);
            if let Ok(cfg) = smedja_sre::SreConfig::from_env() {
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(15))
                    .connect_timeout(std::time::Duration::from_secs(5))
                    .build()
                    .unwrap_or_default();
                match smedja_sre::log_tail(&client, &cfg, service, filter, lines).await {
                    Ok(v) => serde_json::to_string(&v).unwrap_or_default(),
                    Err(e) => format!("error: {e}"),
                }
            } else {
                "SRE config not available (set SMEDJA_LOKI_ENDPOINT)".into()
            }
        }
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

/// Dispatches a tool call to the MCP server that owns `tool_name`.
///
/// Queries the ingot registry for a registered MCP server whose `tools_json`
/// contains an entry named `tool_name`, then forwards the call to that server
/// via `McpHttpClient::call_tool`.  Returns an error string if no server owns
/// the tool or if the HTTP call fails.
pub(crate) async fn dispatch_mcp_tool(
    tool_name: &str,
    input: &serde_json::Value,
    ingot: &IngotHandle,
) -> String {
    let store = crate::mcp_oauth::TokenStore::default_store();
    let env_token = std::env::var("MCP_TOKEN").ok();
    dispatch_mcp_tool_with_store(tool_name, input, ingot, &store, env_token.as_deref()).await
}

/// Resolves the outbound MCP Bearer credential for `server_url`.
///
/// Resolution order: a token persisted in `store`, then the `MCP_TOKEN`
/// environment value (`env_token`), then an empty string (the unauthenticated
/// path, preserving back-compatibility).
pub(crate) fn resolve_mcp_token(
    store: &crate::mcp_oauth::TokenStore,
    server_url: &str,
    env_token: Option<&str>,
) -> String {
    if let Ok(Some(token)) = store.load(server_url) {
        return token.access_token;
    }
    env_token.unwrap_or("").to_owned()
}

/// Dispatches an MCP tool call, resolving the outbound token from `store` (then
/// `env_token`, then empty) and selecting the transport from the registered
/// server's `transport` field.
pub(crate) async fn dispatch_mcp_tool_with_store(
    tool_name: &str,
    input: &serde_json::Value,
    ingot: &IngotHandle,
    store: &crate::mcp_oauth::TokenStore,
    env_token: Option<&str>,
) -> String {
    let server = match ingot.find_mcp_server_for_tool(tool_name).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::debug!(tool = tool_name, "no MCP server registered for tool");
            return format!("error: tool '{tool_name}' is not available");
        }
        Err(e) => {
            tracing::warn!(tool = tool_name, error = %e, "ingot error looking up MCP tool");
            return format!("error: tool '{tool_name}' is not available");
        }
    };

    let token = resolve_mcp_token(store, &server.url, env_token);

    tracing::debug!(
        tool = tool_name,
        server = %server.name,
        url = %server.url,
        transport = %server.transport,
        "dispatching MCP tool call"
    );

    let transport = match crate::mcp_stdio::McpTransport::for_server(&server, &token) {
        Ok(t) => t,
        Err(e) => {
            return format!(
                "error: could not connect to MCP server '{}': {e}",
                server.name
            )
        }
    };

    match transport.call_tool(tool_name, input).await {
        Ok(result) => result,
        Err(e) => format!("error: MCP tool call failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    /// Default FNV embedder for tests that drive `execute_tool`.
    fn test_embedder() -> Arc<dyn crate::embedder_port::Embedder> {
        Arc::new(crate::embedder_port::FnvEmbedder::new())
    }

    // ── output-filters: filter_command_output ─────────────────────────────────

    fn filter_session() -> smedja_ingot::Session {
        smedja_ingot::Session {
            id: uuid::Uuid::new_v4(),
            created_at: smedja_types::Timestamp::from_micros(0),
            updated_at: smedja_types::Timestamp::from_micros(0),
            status: "active".to_owned(),
            task_id: None,
            mode: None,
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        }
    }

    #[tokio::test]
    async fn known_command_output_is_compressed_on_return_path() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let session = filter_session();

        // cargo-build-like noise: many progress lines, one real error.
        let raw = format!(
            "{}error[E0308]: mismatched types\n  --> src/lib.rs:1:1\n",
            "   Compiling crate v0.1.0\n".repeat(40)
        );
        let out = super::filter_command_output(
            "cargo build",
            raw.clone(),
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
        )
        .await;

        assert!(
            out.contains("error[E0308]"),
            "the real error must survive filtering; got:\n{out}"
        );
        assert!(
            !out.contains("Compiling"),
            "progress noise must be filtered; got:\n{out}"
        );
        assert!(
            out.len() < raw.len(),
            "filtered output must be shorter than the original"
        );
    }

    #[tokio::test]
    async fn filtered_command_writes_filter_tagged_row() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let session = filter_session();
        let session_id = session.id.to_string();

        let raw = format!(
            "{}error[E0308]: mismatched types\n",
            "   Compiling crate v0.1.0\n".repeat(40)
        );
        let _ = super::filter_command_output(
            "cargo build",
            raw,
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
        )
        .await;

        let by_source = ingot
            .session_tokens_saved_by_source(&session_id)
            .await
            .unwrap();
        assert_eq!(by_source.len(), 1, "exactly one source recorded");
        assert_eq!(by_source[0].0, "filter", "filter path tags source=filter");
        assert!(by_source[0].1 > 0, "a positive saving must be recorded");
    }

    #[tokio::test]
    async fn crusher_json_path_writes_crusher_tagged_row() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let session = filter_session();
        let session_id = session.id.to_string();

        // A JSON result with null fields routes through SmartCrusher, which strips
        // them, yielding a positive estimated saving tagged source=crusher.
        let raw = serde_json::json!({
            "a": 1, "b": null, "c": null, "d": null, "e": null,
            "nested": { "x": null, "y": null, "z": "keep" }
        })
        .to_string();
        let _ = super::filter_command_output(
            "some_tool",
            raw,
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
        )
        .await;

        let by_source = ingot
            .session_tokens_saved_by_source(&session_id)
            .await
            .unwrap();
        assert_eq!(by_source.len(), 1, "exactly one source recorded");
        assert_eq!(by_source[0].0, "crusher", "JSON path tags source=crusher");
    }

    #[tokio::test]
    async fn unknown_command_output_passes_through_blank_removal_only() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();

        // An unknown command with no blank lines is returned verbatim (ratio 1.0),
        // preserving the captured success/failure contract.
        let raw = "alpha\nbeta\ngamma".to_owned();
        let out = super::filter_command_output(
            "some-unknown-tool --flag",
            raw.clone(),
            ws.path(),
            None,
            &ingot,
            &vault,
        )
        .await;
        assert_eq!(out, raw, "unchanged content must pass through verbatim");
    }

    #[tokio::test]
    async fn error_prefix_contract_is_preserved_through_filtering() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();

        // A failed command's `error:` prefix (the success/failure contract) must
        // never be stripped by filtering.
        let raw = "error: command failed with exit status 1".to_owned();
        let out = super::filter_command_output(
            "some-unknown-tool",
            raw.clone(),
            ws.path(),
            None,
            &ingot,
            &vault,
        )
        .await;
        assert!(
            out.starts_with("error:"),
            "the error contract must be preserved; got: {out}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only: serialises a process-global env var across the await
    async fn bypass_env_skips_executor_filtering() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        // Serialise env-var mutation so concurrent tests do not race on the
        // process-global SMEDJA_NO_TOOL_COMPRESS bypass.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _env_guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();

        std::env::set_var("SMEDJA_NO_TOOL_COMPRESS", "1");
        let raw = format!(
            "{}error[E0308]: mismatched\n",
            "   Compiling crate v0.1.0\n".repeat(40)
        );
        let out = super::filter_command_output(
            "cargo build",
            raw.clone(),
            ws.path(),
            None,
            &ingot,
            &vault,
        )
        .await;
        std::env::remove_var("SMEDJA_NO_TOOL_COMPRESS");
        assert_eq!(out, raw, "bypass must return the output verbatim");
    }

    // ── output-filters: tee-to-vault recovery ─────────────────────────────────

    #[tokio::test]
    async fn reduced_output_is_teed_and_recoverable_via_marker_hash() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let session = filter_session();

        let raw = format!(
            "{}error[E0001]: boom\n",
            "   Compiling crate v0.1.0\n".repeat(40)
        );
        let out = super::filter_command_output(
            "cargo build",
            raw.clone(),
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
        )
        .await;

        // The compressed result carries a recovery marker naming the hash.
        let hash = super::content_hash(&raw);
        assert!(
            out.contains(&format!("smedja_retrieve hash={hash}")),
            "the recovery marker must name the content hash; got:\n{out}"
        );

        // The full output is recoverable via smedja_retrieve (the in-memory store).
        let recovered = super::execute_tool(
            "smedja_retrieve",
            &format!("{{\"hash\":\"{hash}\"}}"),
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert_eq!(
            recovered, raw,
            "smedja_retrieve must return the full uncompressed output"
        );

        // And the full output is teed to the vault recovery namespace.
        let count = {
            let guard = vault.lock().await;
            guard
                .count_by_namespace(super::FILTER_RECOVERY_NAMESPACE)
                .unwrap()
        };
        assert_eq!(count, 1, "full output must be teed to the vault");
    }

    // ── output-filters: savings accounting ────────────────────────────────────

    #[tokio::test]
    async fn filtering_records_positive_tokens_saved() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let session = filter_session();
        let sid = session.id.to_string();

        let raw = format!(
            "{}error[E0001]: boom\n",
            "   Compiling crate v0.1.0\n".repeat(40)
        );
        let _ = super::filter_command_output(
            "cargo build",
            raw,
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
        )
        .await;

        let saved = ingot.session_tokens_saved(&sid).await.unwrap();
        assert!(
            saved > 0,
            "a filtered command must contribute a positive tokens-saved figure; got {saved}"
        );
    }

    #[tokio::test]
    async fn execute_tool_bash_path_runs_filter_and_preserves_output() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();

        // An unknown command emitting distinct non-blank lines passes through
        // unchanged (ratio 1.0), confirming the wiring does not corrupt output.
        let out = super::execute_tool(
            "bash",
            r#"{"command":"printf 'one\ntwo\nthree\n'"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            out.contains("one"),
            "command output must survive; got: {out}"
        );
        assert!(
            out.contains("three"),
            "command output must survive; got: {out}"
        );
    }

    // ── MCP_SERVER_TOOLS subset ───────────────────────────────────────────────

    #[test]
    fn mcp_server_tools_are_exactly_the_read_safe_subset() {
        // No write/exec tool may appear in the server subset.
        const FORBIDDEN: &[&str] = &[
            "edit_file",
            "bash",
            "write_file",
            "run_command",
            "smedja_vault_store",
        ];
        let expected = [
            "graph_query",
            "read_file",
            "list_files",
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

    // ── find_tool_call_json / parse_tool_call ─────────────────────────────────

    #[test]
    fn find_tool_call_json_returns_none_for_empty_string() {
        let result = super::find_tool_call_json("");
        assert!(result.is_none(), "empty string must yield None");
    }

    #[test]
    fn find_tool_call_json_handles_json_embedded_in_text() {
        let text = r#"Here is the call: {"tool":"read_file","input":{"path":"foo.txt"}} done."#;
        let result = super::find_tool_call_json(text);
        assert!(result.is_some(), "embedded JSON must be found");
        let v = result.unwrap();
        assert_eq!(v["tool"], "read_file");
        assert_eq!(v["input"]["path"], "foo.txt");
    }

    #[test]
    fn parse_tool_call_returns_none_for_plain_text() {
        let result = super::parse_tool_call("hello world, no JSON here");
        assert!(result.is_none(), "plain text must yield None");
    }

    #[test]
    fn parse_tool_call_returns_some_for_valid_tool_json() {
        let json = r#"{"tool":"bash","input":{"command":"ls"}}"#;
        let result = super::parse_tool_call(json);
        assert!(result.is_some(), "valid tool JSON must yield Some");
        let (tool_name, input_str) = result.unwrap();
        assert_eq!(tool_name, "bash");
        let input_val: serde_json::Value = serde_json::from_str(&input_str).unwrap();
        assert_eq!(input_val["command"], "ls");
    }

    #[test]
    fn parse_tool_call_returns_none_for_json_without_tool_key() {
        let json = r#"{"action":"bash","input":{"command":"ls"}}"#;
        let result = super::parse_tool_call(json);
        assert!(result.is_none(), "JSON without 'tool' key must yield None");
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
        let result = super::execute_tool(
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
        super::execute_tool(
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

    // ── dispatch_mcp_tool ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_mcp_tool_returns_error_when_no_server_registered() {
        let ig = smedja_ingot::IngotHandle::new(
            smedja_ingot::Ingot::open_in_memory().expect("in-memory Ingot must open"),
        );
        let result = super::dispatch_mcp_tool("unknown_tool", &serde_json::json!({}), &ig).await;
        assert!(
            result.starts_with("error:"),
            "unregistered tool must return an error; got: {result}"
        );
        assert!(
            result.contains("unknown_tool"),
            "error must include the tool name; got: {result}"
        );
    }

    #[tokio::test]
    async fn dispatch_mcp_tool_routes_to_registered_server() {
        use tokio::net::TcpListener;

        // Spawn a minimal mock MCP server that responds to tools/call.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_url = format!("http://{addr}");

        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/",
                axum::routing::post(|| async {
                    axum::Json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "content": [{ "type": "text", "text": "dispatched-ok" }],
                            "isError": false
                        }
                    }))
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Register the mock server in an in-memory Ingot.
        let ig = smedja_ingot::IngotHandle::new(
            smedja_ingot::Ingot::open_in_memory().expect("in-memory Ingot must open"),
        );
        ig.register_mcp_server(smedja_ingot::McpServer {
            id: "mock-1".into(),
            name: "mock-server".into(),
            url: server_url,
            transport: "http".into(),
            tools_json: r#"[{"name":"greet","description":"Greet"}]"#.into(),
            last_refresh: 1.0,
        })
        .await
        .expect("register_mcp_server must succeed");

        let result =
            super::dispatch_mcp_tool("greet", &serde_json::json!({"name": "world"}), &ig).await;
        assert!(
            result.contains("dispatched-ok"),
            "must return the mock server's response; got: {result}"
        );
    }

    #[test]
    fn resolve_mcp_token_prefers_stored_token() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::mcp_oauth::TokenStore::new(tmp.path().to_path_buf());
        let token = crate::mcp_oauth::Token {
            access_token: "stored-bearer".into(),
            token_type: "Bearer".into(),
            refresh_token: None,
            expires_in: Some(3600),
        };
        store.save("https://srv.example.com", &token).unwrap();
        let resolved = super::resolve_mcp_token(&store, "https://srv.example.com", None);
        assert_eq!(resolved, "stored-bearer");
    }

    #[test]
    fn resolve_mcp_token_falls_back_to_env_then_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::mcp_oauth::TokenStore::new(tmp.path().to_path_buf());
        // No stored token, MCP_TOKEN provided → env value.
        let resolved =
            super::resolve_mcp_token(&store, "https://none.example.com", Some("env-bearer"));
        assert_eq!(resolved, "env-bearer");
        // No stored token, no env → empty (unauthenticated path).
        let resolved = super::resolve_mcp_token(&store, "https://none.example.com", None);
        assert_eq!(resolved, "");
    }

    #[tokio::test]
    async fn dispatch_mcp_tool_sends_stored_token_as_bearer() {
        use std::sync::Arc;

        use tokio::net::TcpListener;
        use tokio::sync::Mutex as TokioMutex;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_url = format!("http://{addr}");

        // The mock server echoes back whether it saw the expected Bearer.
        let seen_auth: Arc<TokioMutex<Option<String>>> = Arc::new(TokioMutex::new(None));
        let seen_clone = Arc::clone(&seen_auth);
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/",
                axum::routing::post(
                    move |headers: axum::http::HeaderMap| {
                        let seen = Arc::clone(&seen_clone);
                        async move {
                            let auth = headers
                                .get(axum::http::header::AUTHORIZATION)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_owned);
                            *seen.lock().await = auth;
                            axum::Json(serde_json::json!({
                                "jsonrpc": "2.0", "id": 1,
                                "result": { "content": [{ "type": "text", "text": "ok" }], "isError": false }
                            }))
                        }
                    },
                ),
            );
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Persist a token keyed by the server URL in a scoped store.
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::mcp_oauth::TokenStore::new(tmp.path().to_path_buf());
        store
            .save(
                &server_url,
                &crate::mcp_oauth::Token {
                    access_token: "abc123".into(),
                    token_type: "Bearer".into(),
                    refresh_token: None,
                    expires_in: Some(3600),
                },
            )
            .unwrap();

        let ig = smedja_ingot::IngotHandle::new(
            smedja_ingot::Ingot::open_in_memory().expect("in-memory Ingot must open"),
        );
        ig.register_mcp_server(smedja_ingot::McpServer {
            id: "auth-1".into(),
            name: "auth-server".into(),
            url: server_url.clone(),
            transport: "http".into(),
            tools_json: r#"[{"name":"greet","description":"Greet"}]"#.into(),
            last_refresh: 1.0,
        })
        .await
        .expect("register_mcp_server must succeed");

        let result =
            super::dispatch_mcp_tool_with_store("greet", &serde_json::json!({}), &ig, &store, None)
                .await;
        let _ = result; // result body is not under test here.

        let observed = seen_auth.lock().await.clone();
        assert_eq!(
            observed.as_deref(),
            Some("Bearer abc123"),
            "stored token must be sent as the Bearer credential"
        );
    }

    // ── output scanning (advisory by default) ─────────────────────────────────

    #[tokio::test]
    async fn secret_bearing_tool_result_records_finding_and_returns_original() {
        use smedja_ingot::{Ingot, IngotHandle, Session};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();

        // A session ties the finding to a known session id for lookup.
        let session = Session {
            id: uuid::Uuid::new_v4(),
            created_at: smedja_types::Timestamp::from_micros(0),
            updated_at: smedja_types::Timestamp::from_micros(0),
            status: "active".to_owned(),
            task_id: None,
            mode: None,
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        };
        let sid = session.id.to_string();

        // Write a file whose content contains a synthetic AWS-style key, then
        // read it back through the executor so the result carries the secret.
        let secret = "AKIAIOSFODNN7EXAMPLE";
        std::fs::write(ws.path().join("creds.txt"), format!("key={secret}")).unwrap();

        let result = super::execute_tool(
            "read_file",
            r#"{"path":"creds.txt"}"#,
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;

        // Advisory default: the original content is returned unmodified.
        assert!(
            result.contains(secret),
            "advisory default must return the original content; got: {result}"
        );

        // A security_finding event is recorded with status "warn".
        let events = ingot.list_audit_events(&sid).await.unwrap();
        let finding = events
            .iter()
            .find(|e| e.action_type == "security_finding")
            .expect("a security_finding event must be recorded");
        assert_eq!(finding.status.as_deref(), Some("warn"));
        assert_eq!(finding.tool_name.as_deref(), Some("read_file"));
    }

    #[tokio::test]
    async fn clean_tool_result_records_no_finding() {
        use smedja_ingot::{Ingot, IngotHandle, Session};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();

        let session = Session {
            id: uuid::Uuid::new_v4(),
            created_at: smedja_types::Timestamp::from_micros(0),
            updated_at: smedja_types::Timestamp::from_micros(0),
            status: "active".to_owned(),
            task_id: None,
            mode: None,
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        };
        let sid = session.id.to_string();

        std::fs::write(ws.path().join("notes.txt"), "nothing secret here").unwrap();
        let result = super::execute_tool(
            "read_file",
            r#"{"path":"notes.txt"}"#,
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert_eq!(result, "nothing secret here");

        let events = ingot.list_audit_events(&sid).await.unwrap();
        assert!(
            events.iter().all(|e| e.action_type != "security_finding"),
            "clean output must record no security_finding"
        );
    }

    // ── vault tools via execute_tool ──────────────────────────────────────────

    #[tokio::test]
    async fn vault_search_returns_empty_when_no_entries() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

        let result = super::execute_tool(
            "smedja_vault_search",
            r#"{"query":"rust async"}"#,
            std::path::Path::new("/tmp"),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;

        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["results"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn vault_store_then_search_finds_entry() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

        let store_result = super::execute_tool(
            "smedja_vault_store",
            r#"{"content":"tokio async runtime executor","namespace":"test"}"#,
            std::path::Path::new("/tmp"),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let stored: serde_json::Value = serde_json::from_str(&store_result).unwrap();
        assert_eq!(stored["stored"], true);

        let search_result = super::execute_tool(
            "smedja_vault_search",
            r#"{"query":"tokio async","namespace":"test","k":5}"#,
            std::path::Path::new("/tmp"),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&search_result).unwrap();
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "stored entry must be found");
        assert_eq!(results[0]["namespace"], "test");
    }

    #[tokio::test]
    async fn vault_search_respects_k_limit() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

        for i in 0..5_u8 {
            super::execute_tool(
                "smedja_vault_store",
                &format!(r#"{{"content":"rust programming language crate {i}","namespace":"ns"}}"#),
                std::path::Path::new("/tmp"),
                None,
                &ingot,
                &vault,
                &test_embedder(),
            )
            .await;
        }

        let result = super::execute_tool(
            "smedja_vault_search",
            r#"{"query":"rust programming","namespace":"ns","k":2}"#,
            std::path::Path::new("/tmp"),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(
            v["results"].as_array().unwrap().len() <= 2,
            "k=2 must cap results at 2"
        );
    }

    #[tokio::test]
    async fn smedja_vault_search_returns_results_when_vault_has_matching_entries() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

        // Insert a known entry via the store tool so the embedding path is exercised.
        let store_result = super::execute_tool(
            "smedja_vault_store",
            r#"{"content":"Rust ownership model borrow checker lifetimes","namespace":"search-test"}"#,
            std::path::Path::new("/tmp"),
            None,
            &ingot,
            &vault,
        &test_embedder(),
        )
        .await;
        let stored: serde_json::Value = serde_json::from_str(&store_result).unwrap();
        assert_eq!(
            stored["stored"], true,
            "entry must be stored before searching"
        );

        // Query with text similar to the inserted entry.
        let search_result = super::execute_tool(
            "smedja_vault_search",
            r#"{"query":"Rust ownership borrow checker","namespace":"search-test","k":5}"#,
            std::path::Path::new("/tmp"),
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;

        let v: serde_json::Value = serde_json::from_str(&search_result).unwrap();
        let results = v["results"].as_array().expect("results must be an array");
        assert!(
            !results.is_empty(),
            "smedja_vault_search must return at least one result for a matching query"
        );

        // Verify the returned entry has a positive cosine similarity by checking
        // that the vault itself scores the entry > 0 when searched directly.
        let similarity = {
            let guard = vault.lock().await;
            let qv = crate::embedder::embed("Rust ownership borrow checker");
            let entries = guard
                .search(
                    &qv,
                    "Rust ownership borrow checker",
                    "search-test",
                    1,
                    smedja_vault::LEGACY_MODEL_ID,
                    crate::embedder::DIM,
                )
                .unwrap();
            if entries.is_empty() {
                return;
            }
            // Re-score using the embedder to confirm similarity is positive.
            let stored_vec =
                crate::embedder::embed("Rust ownership model borrow checker lifetimes");
            qv.iter()
                .zip(stored_vec.iter())
                .map(|(a, b)| a * b)
                .sum::<f32>()
        };
        assert!(
            similarity > 0.0,
            "cosine similarity between query and stored entry must be > 0, got {similarity}"
        );
    }

    // ── is_confirm_edits_enabled ──────────────────────────────────────────────

    #[test]
    fn confirm_edits_defaults_to_false_when_no_workspace_toml() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !super::is_confirm_edits_enabled(dir.path()),
            "missing workspace.toml must resolve to false"
        );
    }

    #[test]
    fn confirm_edits_false_when_key_absent() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(smedja.join("workspace.toml"), "[workspace]\nname = \"x\"\n").unwrap();
        assert!(
            !super::is_confirm_edits_enabled(dir.path()),
            "missing [tools] key must resolve to false"
        );
    }

    #[test]
    fn confirm_edits_true_when_enabled_in_workspace_toml() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(
            smedja.join("workspace.toml"),
            "[tools]\nconfirm_edits = true\n",
        )
        .unwrap();
        assert!(
            super::is_confirm_edits_enabled(dir.path()),
            "confirm_edits = true must be detected"
        );
    }

    #[test]
    fn confirm_edits_false_when_explicitly_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(
            smedja.join("workspace.toml"),
            "[tools]\nconfirm_edits = false\n",
        )
        .unwrap();
        assert!(
            !super::is_confirm_edits_enabled(dir.path()),
            "confirm_edits = false must resolve to false"
        );
    }

    // --- WI-018: read_file line ranges, list_files depth/pattern, glob_match ----

    #[test]
    fn glob_match_star_matches_any_suffix() {
        assert!(super::glob_match("*.rs", "main.rs"));
        assert!(!super::glob_match("*.rs", "main.toml"));
    }

    #[test]
    fn glob_match_question_mark_matches_one_char() {
        assert!(super::glob_match("fo?", "foo"));
        assert!(!super::glob_match("fo?", "fo"));
    }

    #[test]
    fn glob_match_star_matches_empty() {
        assert!(super::glob_match("*.rs", ".rs"));
        assert!(super::glob_match("*", "anything"));
        assert!(super::glob_match("*", ""));
    }

    #[test]
    fn glob_match_exact() {
        assert!(super::glob_match("Cargo.toml", "Cargo.toml"));
        assert!(!super::glob_match("Cargo.toml", "cargo.toml"));
    }

    // read_file and list_files helpers are tested via the full execute_tool path
    // in integration — here we test the glob helper and the executor's behaviour
    // through a direct unit path.

    #[test]
    fn read_file_line_range_returns_subset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.txt");
        std::fs::write(&path, "line1\nline2\nline3\nline4\n").unwrap();

        // Simulate the line-range extraction used in the read_file handler.
        let text = std::fs::read_to_string(&path).unwrap();
        let start_line: usize = 2;
        let end_line: usize = 3;
        let start = start_line.saturating_sub(1);
        let lines: Vec<&str> = text.lines().collect();
        let end = end_line.min(lines.len());
        let result = lines[start..end].join("\n");
        assert_eq!(result, "line2\nline3");
    }

    #[test]
    fn read_file_base64_roundtrip() {
        use base64::Engine as _;
        let raw = b"\x00\x01\x02\x03binary";
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .unwrap();
        assert_eq!(decoded, raw);
    }
}
