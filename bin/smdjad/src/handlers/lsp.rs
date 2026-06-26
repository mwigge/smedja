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
    Ok(status_from_snapshot(state.lsp_manager.snapshot()))
}

/// Core of `lsp.status`, parameterised on a snapshot so it is testable without
/// constructing a full [`HandlerState`] or live language server processes.
fn status_from_snapshot(snap: smedja_lsp::LspSnapshot) -> Value {
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
    json!({ "servers": servers })
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

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_lsp::{LspSnapshot, ServerState, ServerStatus};

    fn snapshot_with(servers: Vec<(&str, ServerState)>) -> LspSnapshot {
        LspSnapshot {
            servers: servers
                .into_iter()
                .map(|(name, state)| ServerStatus {
                    name: name.to_owned(),
                    state,
                })
                .collect(),
            diagnostics: Vec::new(),
        }
    }

    // ── lsp.status ────────────────────────────────────────────────────────────

    #[test]
    fn status_empty_snapshot_returns_empty_servers_array() {
        let snap = LspSnapshot::default();
        let resp = status_from_snapshot(snap);
        assert!(resp["servers"].as_array().unwrap().is_empty());
    }

    #[test]
    fn status_ready_server_is_serialised_correctly() {
        let snap = snapshot_with(vec![("rust-analyzer", ServerState::Ready)]);
        let resp = status_from_snapshot(snap);
        let servers = resp["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["name"], "rust-analyzer");
        assert_eq!(servers[0]["state"], "ready");
    }

    #[test]
    fn status_starting_server_is_serialised_correctly() {
        let snap = snapshot_with(vec![("gopls", ServerState::Starting)]);
        let resp = status_from_snapshot(snap);
        let servers = resp["servers"].as_array().unwrap();
        assert_eq!(servers[0]["state"], "starting");
    }

    #[test]
    fn status_degraded_server_includes_reason() {
        let snap = snapshot_with(vec![(
            "pyright",
            ServerState::Degraded("spawn failed".to_owned()),
        )]);
        let resp = status_from_snapshot(snap);
        let state_str = resp["servers"][0]["state"].as_str().unwrap();
        assert_eq!(state_str, "degraded: spawn failed");
    }

    #[test]
    fn status_multiple_servers_all_present() {
        let snap = snapshot_with(vec![
            ("rust-analyzer", ServerState::Ready),
            ("pyright", ServerState::Starting),
            ("gopls", ServerState::Degraded("not found".to_owned())),
        ]);
        let resp = status_from_snapshot(snap);
        let servers = resp["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 3);
        let names: Vec<&str> = servers.iter().map(|v| v["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"rust-analyzer"));
        assert!(names.contains(&"pyright"));
        assert!(names.contains(&"gopls"));
    }
}
