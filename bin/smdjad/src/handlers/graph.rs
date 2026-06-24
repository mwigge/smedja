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

/// Returns the graph database path for a workspace root: `<root>/.smedja/graph.db`.
fn graph_db_path(workspace: &std::path::Path) -> PathBuf {
    workspace.join(".smedja").join("graph.db")
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
