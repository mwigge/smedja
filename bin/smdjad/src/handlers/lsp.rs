//! LSP RPC handlers: `lsp.status`, `lsp.diagnostics`.
//!
//! These expose the daemon-side `LspManager` snapshot over the JSON-RPC socket
//! so `smedja-tui` (and other clients) can query current language-server state
//! and diagnostics without each process managing its own server pool.

use serde_json::{json, Value};
use smedja_rpc::RpcError;

use crate::handlers::HandlerState;

/// Handles `lsp.status`.
///
/// Returns the name and lifecycle state of every active language server.
///
/// Response: `{ servers: [{ name, state }] }`
///   where `state` is one of `"starting"`, `"ready"`, or `"degraded: <reason>"`.
pub(crate) async fn status(state: HandlerState, _params: Value) -> Result<Value, RpcError> {
    let snap = state.lsp_manager.snapshot();
    let servers: Vec<Value> = snap
        .servers
        .iter()
        .map(|s| {
            let state_str = match &s.state {
                smedja_lsp::ServerState::Starting => "starting".to_owned(),
                smedja_lsp::ServerState::Ready => "ready".to_owned(),
                smedja_lsp::ServerState::Degraded(r) => format!("degraded: {r}"),
            };
            json!({ "name": s.name, "state": state_str })
        })
        .collect();
    Ok(json!({ "servers": servers }))
}

/// Handles `lsp.diagnostics`.
///
/// Returns all current diagnostics from all active language servers, sorted
/// by severity (errors first) then file then line.
///
/// Response: `{ diagnostics: [{ file, line, col, severity, code, message }] }`
///   where `severity` is `"error"`, `"warning"`, `"info"`, or `"hint"`.
pub(crate) async fn diagnostics(state: HandlerState, _params: Value) -> Result<Value, RpcError> {
    let snap = state.lsp_manager.snapshot();
    let diags: Vec<Value> = snap
        .diagnostics
        .iter()
        .map(|d| {
            let severity = match d.severity {
                smedja_lsp::Severity::Error => "error",
                smedja_lsp::Severity::Warning => "warning",
                smedja_lsp::Severity::Info => "info",
                smedja_lsp::Severity::Hint => "hint",
            };
            json!({
                "file": d.file.display().to_string(),
                "line": d.line,
                "col": d.col,
                "severity": severity,
                "code": d.code,
                "message": d.message,
            })
        })
        .collect();
    Ok(json!({ "diagnostics": diags }))
}
