//! `session.create` handler and its background workspace re-index helper.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use smedja_ingot::{Session, Task};
use smedja_rpc::RpcError;
use smedja_types::Timestamp;
use uuid::Uuid;

use crate::handlers::HandlerState;
use crate::ingot_err;

/// Handles `session.create`: creates a session (and optional linked task) and
/// kicks off a background workspace re-index when stale.
///
/// # Errors
///
/// Returns an error when an ingot write fails.
#[allow(clippy::too_many_lines)] // session bootstrap + background index kept inline
pub(crate) async fn create(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let lsp_manager = Arc::clone(&state.lsp_manager);
    let pool = Arc::clone(&state.provider_pool);
    let startup_runner = state.startup_runner;
    let startup_model = state.startup_model;
    let title = params
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let mode = params
        .get("mode")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let cowork_mode = params
        .get("cowork_mode")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // The client's working directory (the project repo). Stored on the session
    // and used to root the LSP + code-graph, instead of the daemon's cwd.
    let workspace = params
        .get("workspace")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let task_description: Option<String> = params
        .get("task_description")
        .and_then(Value::as_str)
        .map(str::to_owned);

    // Inherit the runner + model from the most recent prior session so the
    // last-used client (codex→codex) and tier (deep→deep) carry across restarts.
    // `list_sessions` is ordered oldest→newest, so the last entry is the most
    // recent. A session that never overrode the defaults leaves these `None`,
    // which correctly falls back to the startup defaults.
    let (inherited_runner, inherited_model) = ig
        .list_sessions()
        .await
        .ok()
        .and_then(|mut sessions| sessions.pop())
        .map_or((None, None), |s| (s.runner_override, s.model_override));

    let now = Timestamp::now();
    let session_id = Uuid::new_v4();

    // When task_description is provided, create the linked task first so
    // its ID can be stored directly in the Session row.
    let task_id: Option<String> = if let Some(ref desc) = task_description {
        let task = Task {
            id: Uuid::new_v4(),
            title: desc.clone(),
            description: String::new(),
            status: "planned".to_owned(),
            created_at: now,
            session_id: Some(session_id.to_string()),
            response: None,
        };
        ig.create_task(task.clone())
            .await
            .map_err(|e| ingot_err(&e))?;
        Some(task.id.to_string())
    } else {
        None
    };

    let session = Session {
        id: session_id,
        created_at: now,
        updated_at: now,
        status: "active".to_owned(),
        task_id: task_id.clone(),
        mode,
        title: title.clone().unwrap_or_default(),
        cowork_mode,
        workspace_root: workspace.clone(),
        model_override: inherited_model.clone(),
        runner_override: inherited_runner.clone(),
    };

    ig.create_session(session.clone())
        .await
        .map_err(|e| ingot_err(&e))?;

    // Root the LSP and the auto-index at the client's workspace (its repo),
    // falling back to the daemon cwd when none was supplied. This is what makes
    // rust-analyzer start for the project and the graph reflect the right repo,
    // instead of the daemon's $HOME.
    let ws_path = workspace.map_or_else(crate::common::workspace_root, std::path::PathBuf::from);
    lsp_manager.ensure_workspace(ws_path.clone());
    maybe_reindex_workspace(ws_path);

    // When cowork_mode is requested, register the per-session gate.
    // The gate map is owned by build_router; session.create handles the DB flag
    // only here. Callers that need the gate active must also call cowork.set.

    // Effective runner/model = inherited override, else the startup default.
    let effective_runner = inherited_runner.unwrap_or_else(|| startup_runner.to_string());
    let effective_model = inherited_model.unwrap_or_else(|| startup_model.to_string());
    // Derive the tier from the provider pool by (runner, model) so the right
    // label (e.g. "deep") follows the inherited model; fall back to the
    // runner's first entry, then to a coarse heuristic.
    let entries = pool.list_all_entries();
    let tier = entries
        .iter()
        .find(|(r, _, m)| *r == effective_runner && *m == effective_model)
        .or_else(|| entries.iter().find(|(r, _, _)| *r == effective_runner))
        .map_or_else(
            || {
                if effective_runner.contains("local") {
                    "local".to_owned()
                } else {
                    "fast".to_owned()
                }
            },
            |(_, t, _)| t.to_string(),
        );
    Ok(json!({
        "id": session.id,
        "title": title,
        "created_at": session.created_at,
        "cowork_mode": cowork_mode,
        "task_id": task_id,
        "runner": effective_runner,
        "model": effective_model,
        "tier": tier,
    }))
}

/// Triggers a background workspace graph re-index when the workspace has been
/// initialised (`.smedja/workspace.toml` exists) and the graph is stale (older
/// than 24 h, or never indexed).
///
/// Errors are logged and swallowed — re-indexing is advisory and must not fail
/// the `session.create` RPC call that triggers it.
#[allow(clippy::needless_pass_by_value)]
fn maybe_reindex_workspace(cwd: PathBuf) {
    let toml_path = cwd.join(".smedja").join("workspace.toml");
    let needs_index = if toml_path.exists() {
        let content = std::fs::read_to_string(&toml_path).unwrap_or_default();
        if let Ok(parsed) = toml::from_str::<toml::Value>(&content) {
            parsed
                .get("graph")
                .and_then(|g| g.get("last_indexed_at"))
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .is_none_or(|ts| {
                    let age =
                        chrono::Utc::now().signed_duration_since(ts.with_timezone(&chrono::Utc));
                    age.num_hours() >= 24
                })
        } else {
            true
        }
    } else {
        // Only auto-index if workspace.toml already exists (workspace was initialised).
        false
    };

    if needs_index {
        let bg_cwd = cwd.clone();
        let bg_toml = toml_path.clone();
        tokio::task::spawn(async move {
            use opentelemetry::trace::Span as _;
            let tracer = opentelemetry::global::tracer("smedja");
            let mut span = opentelemetry::trace::Tracer::start(&tracer, "smedja.workspace.index");
            let start = std::time::Instant::now();
            let db_path = crate::handlers::graph::graph_db_path(&bg_cwd);
            let bg_cwd_clone = bg_cwd.clone();
            let symbol_count = tokio::task::spawn_blocking(move || {
                smedja_graph::GraphStore::open(&db_path)
                    .and_then(|mut s| {
                        s.index_workspace_incremental(&bg_cwd_clone, "workspace", None)
                    })
                    .unwrap_or(0)
            })
            .await
            .unwrap_or(0);
            let duration_ms = start.elapsed().as_millis();
            span.set_attribute(opentelemetry::KeyValue::new(
                "workspace_path",
                bg_cwd.to_string_lossy().into_owned(),
            ));
            span.set_attribute(opentelemetry::KeyValue::new(
                "symbol_count",
                i64::try_from(symbol_count).unwrap_or(i64::MAX),
            ));
            span.set_attribute(opentelemetry::KeyValue::new(
                "duration_ms",
                i64::try_from(duration_ms).unwrap_or(i64::MAX),
            ));
            span.end();
            let ts = chrono::Utc::now().to_rfc3339();
            let new_content = format!("[graph]\nauto_index = true\nlast_indexed_at = \"{ts}\"\n");
            if let Err(e) = std::fs::write(&bg_toml, new_content) {
                tracing::warn!(error = %e, "failed to update workspace.toml after auto-index");
            }
        });
    }
}
