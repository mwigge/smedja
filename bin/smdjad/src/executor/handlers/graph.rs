//! `graph_query` tool body.

use serde_json::Value;

/// `graph_query` tool body: queries the workspace symbol graph.
///
/// Returns `Err` with an empty-symbol JSON payload when no `graph.db` exists
/// (the original arm exited via `return`, bypassing the output scan); every
/// other outcome is `Ok`.
pub(crate) async fn graph_query(
    input: &Value,
    workspace: &std::path::Path,
) -> Result<String, String> {
    let query = input.get("query").and_then(Value::as_str).unwrap_or("");
    let depth = u8::try_from(input.get("depth").and_then(Value::as_u64).unwrap_or(2)).unwrap_or(2);
    let graph_db_path = crate::handlers::graph::graph_db_path(workspace);
    if !graph_db_path.exists() {
        tracing::debug!("graph.db not found; returning empty symbols");
        return Err(serde_json::json!({ "symbols": [] }).to_string());
    }
    // Opening the graph store and running the query are blocking
    // (SQLite) calls; run them on the blocking pool so they never stall
    // a tokio worker thread.
    let query = query.to_owned();
    let joined =
        tokio::task::spawn_blocking(
            move || match smedja_graph::GraphStore::open(&graph_db_path) {
                Ok(store) => match store.graph_query(&query, 10, depth) {
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
            },
        )
        .await;
    Ok(joined.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "graph_query task join failed");
        serde_json::json!({ "symbols": [] }).to_string()
    }))
}
