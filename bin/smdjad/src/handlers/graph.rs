//! Code-graph RPC handlers: `graph.index`, `graph.query`.
//!
//! These run the server-side [`smedja_graph::GraphStore`] over a workspace.
//! The store wraps a synchronous `rusqlite` connection, so each call runs on a
//! blocking thread via [`tokio::task::spawn_blocking`].

use std::path::PathBuf;

use serde_json::{json, Value};
use smedja_graph::GraphStore;
use smedja_rpc::{codes, RpcError};

use crate::handlers::HandlerState;

/// Default symbol cap for `graph.query` when `limit` is not supplied.
const DEFAULT_QUERY_LIMIT: usize = 50;

/// Resolves the workspace root: the `workspace` param if present and non-empty,
/// else `SMEDJA_WORKSPACE`, else the daemon's current directory.
fn resolve_workspace(params: &Value) -> PathBuf {
    if let Some(ws) = params.get("workspace").and_then(Value::as_str) {
        if !ws.is_empty() {
            return PathBuf::from(ws);
        }
    }
    std::env::var("SMEDJA_WORKSPACE")
        .ok()
        .filter(|p| !p.is_empty())
        .map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            PathBuf::from,
        )
}

/// Returns the graph database path for a workspace root.
///
/// The database lives under the daemon's writable state directory rather than
/// inside the workspace. `smdjad` runs sandboxed (`ProtectSystem=strict`,
/// `ProtectHome=read-only`) with only `~/.config/smedja`, `~/.local/share/smedja`
/// and `$XDG_RUNTIME_DIR` writable, so it cannot create `<workspace>/.smedja`.
/// The path is keyed by a stable SHA-256 of the canonicalised workspace so that
/// `index` and `query` (and the executor/orchestrator readers) always agree:
/// `~/.local/share/smedja/graphs/<hash>/graph.db`.
///
/// Falls back to the in-workspace `<root>/.smedja/graph.db` only when `$HOME`
/// is unset (e.g. unusual unsandboxed callers).
pub(crate) fn graph_db_path(workspace: &std::path::Path) -> PathBuf {
    let Some(home) = crate::dirs_home() else {
        return workspace.join(".smedja").join("graph.db");
    };
    // Canonicalise so different relative/symlinked spellings of the same
    // workspace map to one DB; fall back to the raw path when it cannot be
    // resolved (index and query both hash the same input, so they stay aligned).
    let canonical = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let hash = {
        use sha2::{Digest as _, Sha256};
        format!(
            "{:x}",
            Sha256::digest(canonical.to_string_lossy().as_bytes())
        )
    };
    home.join(".local")
        .join("share")
        .join("smedja")
        .join("graphs")
        .join(hash)
        .join("graph.db")
}

fn graph_err(e: &smedja_graph::GraphError) -> RpcError {
    RpcError::new(codes::INTERNAL_ERROR, e.to_string())
}

fn join_err(e: &tokio::task::JoinError) -> RpcError {
    RpcError::new(codes::INTERNAL_ERROR, format!("graph task panicked: {e}"))
}

/// Handles `graph.index`.
///
/// Params: `{ workspace?: string }`.
/// Response: `{ indexed: <count>, workspace: <path> }`.
///
/// # Errors
///
/// Returns [`codes::INTERNAL_ERROR`] when the `.smedja` directory cannot be
/// created, the store cannot be opened, or indexing fails.
pub(crate) async fn index(_state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let workspace = resolve_workspace(&params);
    let db_path = graph_db_path(&workspace);
    let index_root = workspace.clone();

    let count = tokio::task::spawn_blocking(move || -> Result<usize, RpcError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                RpcError::new(
                    codes::INTERNAL_ERROR,
                    format!("cannot create .smedja directory: {e}"),
                )
            })?;
        }
        let mut store = GraphStore::open(&db_path).map_err(|e| graph_err(&e))?;
        store
            .index_workspace_incremental(&index_root, "workspace", None)
            .map_err(|e| graph_err(&e))
    })
    .await
    .map_err(|e| join_err(&e))??;

    Ok(json!({
        "indexed": count,
        "workspace": workspace.display().to_string(),
    }))
}

/// Handles `graph.query`.
///
/// Params: `{ query: string, depth?: u8, limit?: usize, workspace?: string }`.
/// Response: `{ symbols: [ { id, name, kind, file_path, start_line, end_line, snippet } ] }`.
///
/// # Errors
///
/// Returns [`codes::INVALID_PARAMS`] when `query` is missing, or
/// [`codes::INTERNAL_ERROR`] when the store cannot be opened or the query fails.
pub(crate) async fn query(_state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let query = params
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new(codes::INVALID_PARAMS, "missing required param: query"))?
        .to_owned();
    let depth = params
        .get("depth")
        .and_then(Value::as_u64)
        .and_then(|d| u8::try_from(d).ok())
        .unwrap_or(1);
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|l| usize::try_from(l).ok())
        .unwrap_or(DEFAULT_QUERY_LIMIT);
    let workspace = resolve_workspace(&params);
    let db_path = graph_db_path(&workspace);

    let symbols =
        tokio::task::spawn_blocking(move || -> Result<Vec<smedja_graph::Symbol>, RpcError> {
            let store = GraphStore::open(&db_path).map_err(|e| graph_err(&e))?;
            store
                .graph_query(&query, limit, depth)
                .map_err(|e| graph_err(&e))
        })
        .await
        .map_err(|e| join_err(&e))??;

    let out: Vec<Value> = symbols
        .into_iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "kind": format!("{:?}", s.kind),
                "file_path": s.file_path,
                "start_line": s.start_line,
                "end_line": s.end_line,
                "snippet": s.snippet,
            })
        })
        .collect();

    Ok(json!({ "symbols": out }))
}

/// Handles `graph.status`.
///
/// Params: `{ workspace?: string }`.
/// Response: `{ indexed: <count>, exists: <bool>, workspace: <path> }`.
///
/// Reports the symbol count of the *already-built* graph for a workspace so the
/// TUI can show real status (e.g. after `smj workspace index`) instead of always
/// "graph: /index to build". `exists=false` / `indexed=0` when not yet indexed.
pub(crate) async fn status(_state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let workspace = resolve_workspace(&params);
    let db_path = graph_db_path(&workspace);
    let exists = db_path.exists();
    let count = if exists {
        tokio::task::spawn_blocking(move || -> usize {
            GraphStore::open(&db_path)
                .ok()
                .and_then(|s| s.symbol_count("workspace").ok())
                .unwrap_or(0)
        })
        .await
        .unwrap_or(0)
    } else {
        0
    };
    Ok(json!({
        "indexed": count,
        "exists": exists,
        "workspace": workspace.display().to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A single test owns the HOME env var: separate `#[test]`s would race on it
    // under the parallel test runner.
    #[test]
    fn graph_db_path_is_writable_stable_and_per_workspace() {
        let home = tempfile::tempdir().unwrap();
        let ws1 = tempfile::tempdir().unwrap();
        let ws2 = tempfile::tempdir().unwrap();

        // SAFETY: single-threaded test section; HOME restored before returning.
        let prev_home = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", home.path());
        }

        let a = graph_db_path(ws1.path());
        let b = graph_db_path(ws1.path());
        let other = graph_db_path(ws2.path());

        unsafe {
            match prev_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }

        // Same workspace → same DB path (index and query must agree).
        assert_eq!(a, b);
        // Different workspaces → different DBs.
        assert_ne!(a, other);
        // DB sits under the sandbox-writable state dir, never inside the workspace.
        assert!(
            a.starts_with(
                home.path()
                    .join(".local")
                    .join("share")
                    .join("smedja")
                    .join("graphs")
            ),
            "{a:?}"
        );
        assert!(
            !a.starts_with(ws1.path()),
            "must not live inside the workspace: {a:?}"
        );
        assert_eq!(a.file_name().and_then(|n| n.to_str()), Some("graph.db"));
    }
}
