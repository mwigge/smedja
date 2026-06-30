//! Session RPC handlers:
//! `session.create/list/get/delete/fork/set_model/set_runner/set_mode/set_title/`
//! `context/token_usage/takeover` and `runner.list`.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use smedja_ingot::{Checkpoint, Session, Task};
use smedja_rpc::{codes, RpcError};
use smedja_types::Timestamp;
use smedja_vault::VaultEntry;
use uuid::Uuid;

use crate::handlers::HandlerState;
use crate::{ingot_err, missing_param};

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

/// Handles `session.list`.
///
/// # Errors
///
/// Returns an error when the ingot query fails.
pub(crate) async fn list(state: HandlerState, _params: Value) -> Result<Value, RpcError> {
    list_with(&state.ingot).await
}

/// Core of `session.list`, parameterised on the ingot handle so it is testable
/// without constructing a full [`HandlerState`].
async fn list_with(ig: &smedja_ingot::IngotHandle) -> Result<Value, RpcError> {
    let sessions = ig.list_sessions().await.map_err(|e| ingot_err(&e))?;
    let out: Vec<Value> = sessions
        .into_iter()
        .map(|s| {
            json!({
                "id": s.id,
                "title": s.title,
                "mode": s.mode,
                "runner": s.runner_override,
                "created_at": s.created_at,
                "updated_at": s.updated_at,
            })
        })
        .collect();
    Ok(Value::Array(out))
}

/// Handles `session.get`.
///
/// # Errors
///
/// Returns an error when `id` is missing or the session does not exist.
pub(crate) async fn get(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("id"))?;

    let session = ig
        .get_session(id)
        .await
        .map_err(|e| ingot_err(&e))?
        .ok_or_else(|| RpcError::new(codes::INTERNAL_ERROR, format!("session not found: {id}")))?;

    Ok(json!({
        "id": session.id,
        "title": session.title,
        "mode": session.mode,
        "runner": session.runner_override,
        "created_at": session.created_at,
        "updated_at": session.updated_at,
        "status": session.status,
        "task_id": session.task_id,
        "cowork_mode": session.cowork_mode,
        "active_change": state.active_change.as_deref(),
    }))
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
async fn fork_with(
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

/// Handles `session.token_usage`.
///
/// # Errors
///
/// Returns an error when `session_id` is missing or the ingot query fails.
pub(crate) async fn token_usage(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?;
    let snaps = ig
        .session_token_snapshots(session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    let rows: Vec<Value> = snaps
        .iter()
        .map(|s| {
            json!({
                "turn_n": s.turn_n,
                "input_tok": s.input_tok,
                "output_tok": s.output_tok,
                "cumulative_input": s.cumulative_input,
                "cumulative_output": s.cumulative_output,
            })
        })
        .collect();
    Ok(json!({ "session_id": session_id, "turns": rows }))
}

/// Handles `session.set_model`.
///
/// # Errors
///
/// Returns an error when `session_id`/`model` is missing or the ingot write fails.
pub(crate) async fn set_model(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let model = params["model"]
        .as_str()
        .ok_or_else(|| missing_param("model"))?
        .to_owned();
    ig.update_session_model_override(&session_id, &model)
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "session_id": session_id, "model": model }))
}

/// Parses a runner name (tolerating the `-cli` suffix, e.g. `claude-cli`) into
/// a [`Runner`]. Returns `None` for unknown runners.
fn parse_runner_name(s: &str) -> Option<smedja_assayer::Runner> {
    use smedja_assayer::Runner;
    match s.trim().to_ascii_lowercase().split('-').next()? {
        "claude" => Some(Runner::Claude),
        "codex" => Some(Runner::Codex),
        "local" => Some(Runner::Local),
        "copilot" => Some(Runner::Copilot),
        "minimax" => Some(Runner::Minimax),
        "berget" => Some(Runner::Berget),
        _ => None,
    }
}

/// Parses a tier name into a [`Tier`]. Returns `None` for unknown tiers.
fn parse_tier_name(s: &str) -> Option<smedja_assayer::Tier> {
    use smedja_assayer::Tier;
    match s.trim().to_ascii_lowercase().as_str() {
        "fast" => Some(Tier::Fast),
        "local" => Some(Tier::Local),
        "deep" => Some(Tier::Deep),
        _ => None,
    }
}

/// Handles `session.set_tier`: makes `/tier` meaningful by resolving the
/// session's current runner + the requested tier to a concrete model (via the
/// provider pool) and pinning it as the session's `model_override`. So
/// `/tier deep` actually runs on the runner's deep model (and persists across
/// restarts via the model-override inheritance in `create`).
///
/// # Errors
///
/// Returns an error when `session_id`/`tier` is missing, the tier is unknown,
/// or no model is configured for the (runner, tier) pair.
pub(crate) async fn set_tier(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let pool = state.provider_pool;
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let tier_str = params["tier"]
        .as_str()
        .ok_or_else(|| missing_param("tier"))?
        .to_owned();
    let tier = parse_tier_name(&tier_str)
        .ok_or_else(|| RpcError::new(codes::INVALID_PARAMS, format!("unknown tier: {tier_str}")))?;

    // Resolve the session's effective runner (override, else the startup default).
    let runner_str = ig
        .get_session(&session_id)
        .await
        .ok()
        .flatten()
        .and_then(|s| s.runner_override)
        .unwrap_or_else(|| state.startup_runner.to_string());
    let runner = parse_runner_name(&runner_str).ok_or_else(|| {
        RpcError::new(
            codes::INVALID_PARAMS,
            format!("unknown runner: {runner_str}"),
        )
    })?;

    // (runner, tier) → model, falling back through the eligible ring.
    let model = pool
        .get(runner, tier)
        .or_else(|| pool.eligible_ring(runner, tier).into_iter().next())
        .map(|e| e.default_model.clone())
        .ok_or_else(|| {
            RpcError::new(
                codes::INVALID_PARAMS,
                format!("no model configured for {runner_str} @ {tier_str}"),
            )
        })?;

    ig.update_session_model_override(&session_id, &model)
        .await
        .map_err(|e| ingot_err(&e))?;

    Ok(json!({
        "session_id": session_id,
        "tier": tier_str,
        "runner": runner_str,
        "model": model,
    }))
}

/// Handles `session.set_runner`.
///
/// # Errors
///
/// Returns an error when `session_id`/`runner` is missing, the runner is unknown,
/// or the ingot write fails.
pub(crate) async fn set_runner(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let runner_str = params["runner"]
        .as_str()
        .ok_or_else(|| missing_param("runner"))?
        .to_owned();
    // Validate and normalise to the canonical key stored in the DB.
    let canonical = crate::common::parse_runner_str(&runner_str)
        .map(crate::common::runner_session_key)
        .ok_or_else(|| {
            RpcError::new(
                codes::INVALID_PARAMS,
                format!("unknown runner: {runner_str}; valid: claude, codex, local, copilot"),
            )
        })?;
    ig.update_session_runner_override(&session_id, canonical)
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "session_id": session_id, "runner": canonical }))
}

/// Handles `session.set_title`: overwrites the session's human-readable title.
///
/// # Errors
///
/// Returns an error when `session_id`/`title` is missing or the ingot write fails.
pub(crate) async fn set_title(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params["session_id"]
        .as_str()
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let title = params["title"]
        .as_str()
        .ok_or_else(|| missing_param("title"))?
        .to_owned();
    ig.update_session_title(&session_id, &title)
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "session_id": session_id, "title": title }))
}

/// Handles `session.set_mode`.
///
/// # Errors
///
/// Returns an error when `session_id`/`mode` is missing, the session is a
/// read-only review session, or the ingot write fails.
pub(crate) async fn set_mode(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let mode = params
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("mode"))?
        .to_owned();
    // Prevent escalation out of read-only review sessions.
    let existing_session = ig
        .get_session(&session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    if let Some(existing_session) = existing_session {
        if existing_session.mode.as_deref() == Some("review") {
            return Err(RpcError::new(
                codes::INVALID_PARAMS,
                "review sessions are read-only",
            ));
        }
    }
    ig.update_session_mode(&session_id, &mode)
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "session_id": session_id, "mode": mode }))
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

/// Handles `runner.list`.
///
/// # Errors
///
/// Infallible in practice; the signature matches the handler contract.
#[allow(clippy::unused_async)] // uniform handler signature: all handlers are async fns
pub(crate) async fn runner_list(state: HandlerState, _params: Value) -> Result<Value, RpcError> {
    let pool = state.provider_pool;
    let runners: Vec<Value> = pool
        .list_all_entries()
        .into_iter()
        .map(|(runner, tier, model)| json!({ "runner": runner, "tier": tier, "model": model }))
        .collect();
    Ok(json!({ "runners": runners }))
}

/// Handles `session.context`: token-window usage plus vault warm/cold counts.
///
/// # Errors
///
/// Returns an error when `session_id` is missing or an ingot query fails.
pub(crate) async fn context(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let pt = state.price_table;
    let vt = state.vault;
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?;
    let snaps = ig
        .session_token_snapshots(session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    let (cumulative_input, cumulative_output) = snaps
        .last()
        .map_or((0i64, 0i64), |s| (s.cumulative_input, s.cumulative_output));
    let used_tok = cumulative_input.saturating_add(cumulative_output);
    let model = ig
        .session_last_model(session_id)
        .await
        .map_err(|e| ingot_err(&e))?
        .unwrap_or_default();
    let window_tok = u64::from(pt.context_window(&model));
    let vt = Arc::clone(&vt);
    let (vault_warm_count, vault_cold_count) = tokio::task::spawn_blocking(move || {
        let guard = vt.blocking_lock();
        let warm = guard.count_by_namespace("warm").unwrap_or(0);
        let cold = guard.count_by_namespace("default").unwrap_or(0);
        (warm, cold)
    })
    .await
    .unwrap_or((0, 0));
    Ok(json!({
        "session_id": session_id,
        "used_tok": used_tok,
        "window_tok": window_tok,
        "model": model,
        "vault_warm_count": vault_warm_count,
        "vault_cold_count": vault_cold_count,
    }))
}

/// Handles `session.history`: returns the ordered turn/message records for a
/// session, sourced from the ingot's checkpoint blobs and audit trail.
///
/// Params: `{ session_id: string }`.
/// Response: `{ session_id, turns: [ { turn_n, created_at, messages } ], audit: [ … ] }`
/// where `turns` is ordered by `turn_n` ascending (each carries the conversation
/// snapshot for that turn) and `audit` is the ordered tool/turn audit trail.
///
/// # Errors
///
/// Returns an error when `session_id` is missing or an ingot read fails.
pub(crate) async fn history(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();

    let checkpoints = ig
        .list_checkpoints(&session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    let turns: Vec<Value> = checkpoints
        .iter()
        .map(|cp| {
            // The messages blob is stored as a JSON array; surface it parsed so
            // callers receive structured records rather than an escaped string.
            let messages: Value =
                serde_json::from_str(&cp.messages_json).unwrap_or(Value::Array(Vec::new()));
            json!({
                "turn_n": cp.turn_n,
                "created_at": cp.created_at,
                "messages": messages,
            })
        })
        .collect();

    let audit = ig
        .list_audit_events(&session_id)
        .await
        .map_err(|e| ingot_err(&e))?;
    let audit_json: Vec<Value> = audit
        .into_iter()
        .map(|ev| serde_json::to_value(&ev).unwrap_or(Value::Null))
        .collect();

    Ok(json!({
        "session_id": session_id,
        "turns": turns,
        "audit": audit_json,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_ingot::{Ingot, IngotHandle};

    #[test]
    fn parse_runner_name_tolerates_cli_suffix_and_rejects_unknown() {
        use smedja_assayer::Runner;
        assert_eq!(parse_runner_name("claude"), Some(Runner::Claude));
        assert_eq!(parse_runner_name("claude-cli"), Some(Runner::Claude));
        assert_eq!(parse_runner_name("codex-cli"), Some(Runner::Codex));
        assert_eq!(parse_runner_name("LOCAL"), Some(Runner::Local));
        assert_eq!(parse_runner_name("minimax"), Some(Runner::Minimax));
        assert_eq!(parse_runner_name("nope"), None);
    }

    #[test]
    fn parse_tier_name_maps_known_tiers() {
        use smedja_assayer::Tier;
        assert_eq!(parse_tier_name("fast"), Some(Tier::Fast));
        assert_eq!(parse_tier_name("deep"), Some(Tier::Deep));
        assert_eq!(parse_tier_name("local"), Some(Tier::Local));
        assert_eq!(parse_tier_name("ultra"), None);
    }

    fn handle() -> IngotHandle {
        IngotHandle::new(Ingot::open_in_memory().unwrap())
    }

    fn sample_session(id: Uuid, title: &str) -> Session {
        let now = Timestamp::now();
        Session {
            id,
            created_at: now,
            updated_at: now,
            status: "active".to_owned(),
            task_id: None,
            mode: None,
            title: title.to_owned(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        }
    }

    // ── session.list ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn list_returns_empty_when_no_sessions() {
        let ig = handle();
        let resp = list_with(&ig).await.unwrap();
        assert_eq!(resp, Value::Array(vec![]));
    }

    #[tokio::test]
    async fn list_returns_all_created_sessions() {
        let ig = handle();
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        ig.create_session(sample_session(id_a, "alpha"))
            .await
            .unwrap();
        ig.create_session(sample_session(id_b, "beta"))
            .await
            .unwrap();

        let resp = list_with(&ig).await.unwrap();
        let arr = resp.as_array().unwrap();
        assert_eq!(arr.len(), 2, "expected two sessions");
        let titles: Vec<&str> = arr.iter().map(|v| v["title"].as_str().unwrap()).collect();
        assert!(titles.contains(&"alpha"), "missing 'alpha'");
        assert!(titles.contains(&"beta"), "missing 'beta'");
    }

    // ── session.fork ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fork_creates_new_session_with_same_title() {
        let ig = handle();
        let parent_id = Uuid::new_v4();
        ig.create_session(sample_session(parent_id, "my-session"))
            .await
            .unwrap();

        let resp = fork_with(&ig, parent_id.to_string(), None).await.unwrap();

        // The response reports the new session id and the parent.
        assert_eq!(resp["forked_from"], parent_id.to_string());
        let new_id = resp["session_id"].as_str().unwrap();
        assert_ne!(new_id, parent_id.to_string(), "forked id must differ");

        // The new session must exist in the store with the same title.
        let new_sess = ig.get_session(new_id).await.unwrap().unwrap();
        assert_eq!(new_sess.title, "my-session");
        assert_eq!(new_sess.status, "active");
    }

    #[tokio::test]
    async fn fork_has_checkpoint_false_when_no_checkpoint() {
        let ig = handle();
        let parent_id = Uuid::new_v4();
        ig.create_session(sample_session(parent_id, "s"))
            .await
            .unwrap();

        let resp = fork_with(&ig, parent_id.to_string(), None).await.unwrap();
        assert_eq!(resp["has_checkpoint"], false);
    }

    #[tokio::test]
    async fn fork_copies_checkpoint_into_forked_session() {
        let ig = handle();
        let parent_id = Uuid::new_v4();
        ig.create_session(sample_session(parent_id, "s"))
            .await
            .unwrap();
        ig.save_checkpoint(Checkpoint {
            id: Uuid::new_v4(),
            session_id: parent_id.to_string(),
            turn_n: 3,
            messages_json: r#"["hello"]"#.to_owned(),
            created_at: Timestamp::now(),
            compaction_id: None,
        })
        .await
        .unwrap();

        let resp = fork_with(&ig, parent_id.to_string(), None).await.unwrap();
        assert_eq!(resp["has_checkpoint"], true, "checkpoint should be copied");

        let new_id = resp["session_id"].as_str().unwrap();
        let cp = ig.latest_checkpoint(new_id).await.unwrap();
        assert!(cp.is_some(), "forked session must have a checkpoint");
        assert_eq!(cp.unwrap().turn_n, 3);
    }

    #[tokio::test]
    async fn fork_returns_error_for_unknown_session() {
        let ig = handle();
        let err = fork_with(&ig, "no-such-id".to_owned(), None)
            .await
            .unwrap_err();
        assert_eq!(err.code, smedja_rpc::codes::INTERNAL_ERROR);
    }

    // --- WI-018 GAP C: session.fork at arbitrary turn_n ----------------------

    #[tokio::test]
    async fn fork_at_turn_n_selects_closest_checkpoint() {
        let ig = handle();
        let parent_id = Uuid::new_v4();
        ig.create_session(sample_session(parent_id, "s"))
            .await
            .unwrap();
        for turn in [1i64, 3, 5] {
            ig.save_checkpoint(Checkpoint {
                id: Uuid::new_v4(),
                session_id: parent_id.to_string(),
                turn_n: turn,
                messages_json: format!(r#"["turn-{turn}"]"#),
                created_at: Timestamp::now(),
                compaction_id: None,
            })
            .await
            .unwrap();
        }

        // Fork at turn 4 → closest checkpoint not exceeding 4 is turn 3.
        let resp = fork_with(&ig, parent_id.to_string(), Some(4))
            .await
            .unwrap();
        assert_eq!(resp["has_checkpoint"], true);
        let new_id = resp["session_id"].as_str().unwrap();
        let cp = ig.latest_checkpoint(new_id).await.unwrap().unwrap();
        assert_eq!(
            cp.turn_n, 3,
            "expected checkpoint at turn 3, got {}",
            cp.turn_n
        );
    }

    #[tokio::test]
    async fn fork_at_turn_n_past_last_returns_error() {
        let ig = handle();
        let parent_id = Uuid::new_v4();
        ig.create_session(sample_session(parent_id, "s"))
            .await
            .unwrap();
        // No checkpoints exist.
        let err = fork_with(&ig, parent_id.to_string(), Some(99))
            .await
            .unwrap_err();
        assert_eq!(
            err.code,
            smedja_rpc::codes::INTERNAL_ERROR,
            "must error when no checkpoints"
        );
    }

    #[tokio::test]
    async fn fork_at_turn_n_before_all_checkpoints_returns_error() {
        let ig = handle();
        let parent_id = Uuid::new_v4();
        ig.create_session(sample_session(parent_id, "s"))
            .await
            .unwrap();
        ig.save_checkpoint(Checkpoint {
            id: Uuid::new_v4(),
            session_id: parent_id.to_string(),
            turn_n: 5,
            messages_json: r#"["hello"]"#.to_owned(),
            created_at: Timestamp::now(),
            compaction_id: None,
        })
        .await
        .unwrap();
        // Request turn 2 but the only checkpoint is at turn 5.
        let err = fork_with(&ig, parent_id.to_string(), Some(2))
            .await
            .unwrap_err();
        assert_eq!(
            err.code,
            smedja_rpc::codes::INTERNAL_ERROR,
            "must error when no checkpoint <= requested turn"
        );
    }
}
