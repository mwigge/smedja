//! Session lifecycle handlers: create (with background workspace re-index),
//! delete, fork, and takeover. Moved verbatim from `session.rs`.

use super::*;

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

    // Resolve the ONE canonical active repository for this session up front: the
    // git root enclosing the client's workspace (or the daemon's default when no
    // workspace was supplied). Storing this absolute canonical path on the
    // session — and reusing it verbatim below for the LSP root and auto-index —
    // makes `/index`, `graph.query`, and per-turn injection all hash the same
    // `graph_db_path`, and makes a subdir launch index the repo root, not the
    // subdir.
    let active_repo = match workspace.as_deref() {
        Some(w) => crate::common::resolve_active_repo(std::path::Path::new(w)),
        None => crate::common::workspace_root(),
    };
    let active_repo_str = active_repo.to_string_lossy().into_owned();

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
        workspace_root: Some(active_repo_str),
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
    lsp_manager.ensure_workspace(active_repo.clone());
    maybe_reindex_workspace(active_repo);

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

/// Path of the auto-index staleness marker for a repo's graph DB.
///
/// It lives *next to* the graph DB under the daemon's writable state dir
/// (`~/.local/share/smedja/graphs/<hash>/last_indexed_at`), keyed by the git
/// root through the same hash `graph_db_path` uses. Keeping it there — rather
/// than in `<repo>/.smedja/workspace.toml` — is what lets the sandboxed daemon
/// (`ProtectHome=read-only`) record indexing progress at all, and ties the
/// staleness check to the exact DB the index/query/injection paths read.
fn index_marker_path(repo_root: &std::path::Path) -> Option<PathBuf> {
    crate::handlers::graph::graph_db_path(repo_root)
        .parent()
        .map(|p| p.join("last_indexed_at"))
}

/// Returns whether the repo's graph should be (re-)indexed: `true` when it has
/// never been indexed (no marker) or the last index is at least 24 h old.
///
/// Crucially there is **no** `.smedja/workspace.toml` precondition: a repo with
/// no marker yet is treated as stale, so the first `session.create` for it
/// triggers an auto-index.
fn graph_is_stale(repo_root: &std::path::Path) -> bool {
    let content = index_marker_path(repo_root).and_then(|m| std::fs::read_to_string(m).ok());
    marker_is_stale(content.as_deref())
}

/// Pure staleness decision over a marker's contents: `None` (never indexed) or an
/// unparseable / ≥24 h-old RFC 3339 timestamp is stale.
fn marker_is_stale(content: Option<&str>) -> bool {
    let Some(content) = content else {
        return true; // never indexed → index now
    };
    chrono::DateTime::parse_from_rfc3339(content.trim())
        .ok()
        .is_none_or(|ts| {
            let age = chrono::Utc::now().signed_duration_since(ts.with_timezone(&chrono::Utc));
            age.num_hours() >= 24
        })
}

/// Triggers a background graph index for the active repo on `session.create`.
///
/// Unlike the previous behaviour, this no longer requires a pre-existing
/// `.smedja/workspace.toml`: the first session opened for a repository indexes
/// it (bounded, in the background). A 24 h staleness marker next to the graph DB
/// avoids re-indexing on every session. The index itself is incremental
/// (mtime-based), so a re-index only re-parses changed files.
///
/// Errors are logged and swallowed — indexing is advisory and must not fail the
/// `session.create` RPC that triggers it.
#[allow(clippy::needless_pass_by_value)]
fn maybe_reindex_workspace(repo_root: PathBuf) {
    if !graph_is_stale(&repo_root) {
        return;
    }

    tokio::task::spawn(async move {
        use opentelemetry::trace::Span as _;
        let tracer = opentelemetry::global::tracer("smedja");
        let mut span = opentelemetry::trace::Tracer::start(&tracer, "smedja.workspace.index");
        let start = std::time::Instant::now();
        let db_path = crate::handlers::graph::graph_db_path(&repo_root);
        let marker = index_marker_path(&repo_root);
        let index_root = repo_root.clone();
        let symbol_count = tokio::task::spawn_blocking(move || {
            if let Some(parent) = db_path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!(error = %e, "auto-index: cannot create graph dir");
                    return 0;
                }
            }
            smedja_graph::GraphStore::open(&db_path)
                .and_then(|mut s| s.index_workspace_incremental(&index_root, "workspace"))
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "auto-index failed");
                    0
                })
        })
        .await
        .unwrap_or(0);
        let duration_ms = start.elapsed().as_millis();
        span.set_attribute(opentelemetry::KeyValue::new(
            "workspace_path",
            repo_root.to_string_lossy().into_owned(),
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
        // Record completion so the 24 h staleness check can skip the next call.
        if let Some(marker) = marker {
            let ts = chrono::Utc::now().to_rfc3339();
            if let Err(e) = std::fs::write(&marker, ts) {
                tracing::warn!(error = %e, "failed to write auto-index marker");
            }
        }
    });
}

/// Handles `session.delete`.
///
/// # Errors
///
/// Returns an error when `id` is missing or the ingot write fails.
pub(crate) async fn delete(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("id"))?;

    ig.delete_session(id).await.map_err(|e| ingot_err(&e))?;
    Ok(Value::Bool(true))
}

/// Handles `session.fork`.
///
/// # Errors
///
/// Returns an error when `session_id` is missing, the session does not exist, or
/// an ingot write fails.
pub(crate) async fn fork(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let turn_n = params
        .get("turn_n")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    fork_with(&state.ingot, session_id, turn_n).await
}

/// Core of `session.fork`, parameterised on the ingot handle so it is testable
/// without constructing a full [`HandlerState`].
///
/// When `turn_n` is `Some`, the checkpoint closest to (and not exceeding) that
/// turn is used instead of the latest checkpoint. Returns an error if `turn_n`
/// is provided but no checkpoints exist for the session.
pub(crate) async fn fork_with(
    ig: &smedja_ingot::IngotHandle,
    session_id: String,
    turn_n: Option<u32>,
) -> Result<Value, RpcError> {
    // Each DB call acquires and immediately releases the lock so other
    // concurrent RPC handlers (including turn.subscribe's polling loop)
    // are not serialised behind the entire fork sequence.
    let parent = {
        ig.get_session(&session_id)
            .await
            .map_err(|e| ingot_err(&e))?
            .ok_or_else(|| {
                RpcError::new(
                    codes::INTERNAL_ERROR,
                    format!("session not found: {session_id}"),
                )
            })?
    };

    let selected_cp = if let Some(target_turn) = turn_n {
        // Find the checkpoint with the largest turn_n that does not exceed target_turn.
        let all_cps = ig
            .list_checkpoints(&session_id)
            .await
            .map_err(|e| ingot_err(&e))?;
        if all_cps.is_empty() {
            return Err(RpcError::new(
                codes::INTERNAL_ERROR,
                format!(
                    "no checkpoints for session {session_id}; cannot fork at turn {target_turn}"
                ),
            ));
        }
        let target = i64::from(target_turn);
        let cp = all_cps
            .into_iter()
            .filter(|c| c.turn_n <= target)
            .max_by_key(|c| c.turn_n)
            .ok_or_else(|| {
                RpcError::new(
                    codes::INTERNAL_ERROR,
                    format!(
                        "no checkpoint at or before turn {target_turn} for session {session_id}"
                    ),
                )
            })?;
        Some(cp)
    } else {
        ig.latest_checkpoint(&session_id)
            .await
            .map_err(|e| ingot_err(&e))?
    };

    let now = Timestamp::now();
    let new_id = Uuid::new_v4().to_string();

    {
        ig.create_session(Session {
            id: Uuid::parse_str(&new_id)
                .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("uuid error: {e}")))?,
            created_at: now,
            updated_at: now,
            status: "active".into(),
            task_id: None,
            mode: parent.mode.clone(),
            title: parent.title.clone(),
            cowork_mode: parent.cowork_mode,
            workspace_root: parent.workspace_root.clone(),
            model_override: parent.model_override.clone(),
            runner_override: None,
        })
        .await
        .map_err(|e| ingot_err(&e))?;
    }

    let has_checkpoint = selected_cp.is_some();
    if let Some(cp) = selected_cp {
        ig.save_checkpoint(Checkpoint {
            id: Uuid::new_v4(),
            session_id: new_id.clone(),
            turn_n: cp.turn_n,
            messages_json: cp.messages_json,
            created_at: now,
            compaction_id: cp.compaction_id,
        })
        .await
        .map_err(|e| ingot_err(&e))?;
    }

    Ok(json!({
        "session_id": new_id,
        "forked_from": session_id,
        "has_checkpoint": has_checkpoint,
    }))
}

/// Handles `session.takeover`: forks a session onto a new runner atomically.
///
/// # Errors
///
/// Returns an error when `session_id`/`runner` is missing, the runner is unknown,
/// the session does not exist, or an ingot write fails.
#[allow(clippy::too_many_lines)] // single atomic takeover pipeline kept inline
pub(crate) async fn takeover(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let vt = state.vault;
    let embedder = state.embedder;
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let runner_str = params["runner"]
        .as_str()
        .ok_or_else(|| missing_param("runner"))?
        .to_owned();

    let canonical = crate::common::parse_runner_str(&runner_str)
        .map(crate::common::runner_session_key)
        .ok_or_else(|| {
            RpcError::new(
                codes::INVALID_PARAMS,
                format!("unknown runner: {runner_str}; valid: claude, codex, local, copilot"),
            )
        })?;

    let parent = {
        ig.get_session(&session_id)
            .await
            .map_err(|e| ingot_err(&e))?
            .ok_or_else(|| {
                RpcError::new(
                    codes::INTERNAL_ERROR,
                    format!("session not found: {session_id}"),
                )
            })?
    };

    let latest_cp = {
        ig.latest_checkpoint(&session_id)
            .await
            .map_err(|e| ingot_err(&e))?
    };

    let now = Timestamp::now();
    let new_id = Uuid::new_v4().to_string();

    {
        ig.create_session(Session {
            id: Uuid::parse_str(&new_id)
                .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("uuid error: {e}")))?,
            created_at: now,
            updated_at: now,
            status: "active".into(),
            task_id: None,
            mode: parent.mode.clone(),
            title: parent.title.clone(),
            cowork_mode: parent.cowork_mode,
            workspace_root: parent.workspace_root.clone(),
            model_override: parent.model_override.clone(),
            runner_override: Some(canonical.to_owned()),
        })
        .await
        .map_err(|e| ingot_err(&e))?;
    }

    let has_checkpoint = latest_cp.is_some();
    let handoff_context_id = format!("handoff:{session_id}:{new_id}");
    if let Some(cp) = latest_cp {
        ig.save_checkpoint(Checkpoint {
            id: Uuid::new_v4(),
            session_id: new_id.clone(),
            turn_n: cp.turn_n,
            messages_json: cp.messages_json.clone(),
            created_at: now,
            compaction_id: cp.compaction_id.clone(),
        })
        .await
        .map_err(|e| ingot_err(&e))?;

        // Fire-and-forget vault write so the receiving session can retrieve
        // the handoff context via smedja_vault_search namespace="handoff".
        let hid = handoff_context_id.clone();
        let from_sid = session_id.clone();
        let to_sid = new_id.clone();
        let runner_str = canonical.to_owned();
        let messages = cp.messages_json.clone();
        let embedding = embedder.embed_query(&messages).await;
        let model_id = embedder.model_id().to_owned();
        let dim = embedder.dim();
        tokio::task::spawn_blocking(move || {
            let entry = VaultEntry {
                id: hid.clone(),
                embedding,
                payload: serde_json::json!({
                    "from_session_id": from_sid,
                    "to_session_id": to_sid,
                    "runner": runner_str,
                }),
                namespace: "handoff".to_owned(),
                content: messages,
                source_file: None,
                added_by: Some("session.takeover".to_owned()),
                chunk_index: None,
                parent_id: None,
                created_at: 0.0,
                embedder_model_id: model_id,
                dim,
            };
            let mut guard = vt.blocking_lock();
            let _ = guard.upsert(&entry);
        });
    }

    Ok(json!({
        "new_session_id": new_id,
        "forked_from": session_id,
        "runner": canonical,
        "has_checkpoint": has_checkpoint,
        "context_namespace": "handoff",
        "context_id": handoff_context_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::{graph_is_stale, index_marker_path, marker_is_stale};

    #[test]
    fn marker_absent_is_stale_no_workspace_toml_required() {
        // The whole point of the new behaviour: with no marker (and no
        // workspace.toml anywhere), the repo is stale so auto-index fires on the
        // first session.create.
        assert!(marker_is_stale(None), "never-indexed repo must be stale");
    }

    #[test]
    fn marker_fresh_is_not_stale_but_old_is() {
        let fresh = chrono::Utc::now().to_rfc3339();
        assert!(
            !marker_is_stale(Some(&fresh)),
            "just-indexed repo must not be re-indexed"
        );

        let old = (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
        assert!(marker_is_stale(Some(&old)), "≥24h-old index must be stale");

        // Whitespace tolerance + garbage → treated as stale (re-index).
        assert!(!marker_is_stale(Some(&format!("  {fresh}\n"))));
        assert!(marker_is_stale(Some("not-a-timestamp")));
    }

    // `graph_is_stale` reads the marker via `index_marker_path`. When the marker
    // (and thus its graphs/<hash> dir) does not exist, the repo is stale — no
    // `.smedja/workspace.toml` is consulted. Uses a $HOME-independent path check:
    // `index_marker_path` returns a path only when a home dir is resolvable, so
    // we assert the marker basename and the never-indexed staleness together.
    #[test]
    fn graph_is_stale_true_for_never_indexed_repo() {
        let repo = tempfile::tempdir().unwrap();
        // No marker has ever been written for this fresh temp repo → stale.
        assert!(graph_is_stale(repo.path()));
        if let Some(marker) = index_marker_path(repo.path()) {
            assert_eq!(
                marker.file_name().and_then(|n| n.to_str()),
                Some("last_indexed_at")
            );
            // The marker never lives inside the workspace (sandbox-writable dir).
            assert!(!marker.starts_with(repo.path()), "{marker:?}");
        }
    }
}
