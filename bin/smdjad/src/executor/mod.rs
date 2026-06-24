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

use serde_json::Value;
use smedja_ingot::{IngotHandle, Session};
use smedja_vault::{Vault, VaultEntry};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::sandbox::SandboxExecutor;

mod fs_tools;
use fs_tools::{
    assert_within_workspace, extract_proposed_content, read_current_content, role_allows_write_bash,
};

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

/// In-memory store for content blocks addressed by SHA-256 hash.
/// Used by the `smedja_retrieve` tool to look up compressed context blocks.
fn retrieve_store() -> &'static tokio::sync::Mutex<HashMap<String, String>> {
    static STORE: OnceLock<tokio::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
    STORE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
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

            // SandboxExecutor: use Docker sandbox when configured and tool is not exempt.
            let sandbox = SandboxExecutor::new();
            if sandbox.available && !SandboxExecutor::is_exempt(tool_name) {
                match sandbox.exec(cmd, workspace).await {
                    Ok(out) => out,
                    Err(e) => format!("error: {e}"),
                }
            } else {
                super::exec_bash(cmd, workspace).await
            }
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
            match tokio::fs::read_to_string(&full).await {
                Ok(contents) => contents,
                Err(e) => format!("error reading {path_str}: {e}"),
            }
        }
        "list_files" => {
            let dir_str = input.get("path").and_then(Value::as_str).unwrap_or(".");
            let full = match assert_within_workspace(workspace, dir_str) {
                Ok(p) => p,
                Err(err) => return err,
            };
            match tokio::fs::read_dir(&full).await {
                Ok(mut rd) => {
                    let mut entries = Vec::new();
                    while let Ok(Some(entry)) = rd.next_entry().await {
                        entries.push(entry.file_name().to_string_lossy().into_owned());
                    }
                    entries.join("\n")
                }
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
            tokio::task::spawn_blocking(move || {
                let query_vec = crate::embedder::embed(&query_text);
                let guard = vault.blocking_lock();
                match guard.search(&query_vec, &query_text, &ns, k) {
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
            tokio::task::spawn_blocking(move || {
                let embedding = crate::embedder::embed(&content);
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
            let graph_db_path = workspace.join(".smedja").join("graph.db");
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
                let client = reqwest::Client::new();
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
                let client = reqwest::Client::new();
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
                let client = reqwest::Client::new();
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

    let client = match crate::mcp_http::McpHttpClient::new(&server.url, "") {
        Ok(c) => c,
        Err(e) => {
            return format!(
                "error: could not connect to MCP server '{}': {e}",
                server.name
            )
        }
    };

    tracing::debug!(
        tool = tool_name,
        server = %server.name,
        url = %server.url,
        "dispatching MCP tool call"
    );

    match client.call_tool(tool_name, input).await {
        Ok(result) => result,
        Err(e) => format!("error: MCP tool call failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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
                .search(&qv, "Rust ownership borrow checker", "search-test", 1)
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
}
