//! Task RPC handlers: `task.get/list/create/close/parallel/cancel`.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use smedja_ingot::Task;
use smedja_rpc::{codes, RpcError};
use smedja_types::Timestamp;
use smedja_vault::VaultEntry;
use uuid::Uuid;

use crate::handlers::HandlerState;
use crate::{ingot_err, missing_param};

/// Handles `task.get`.
///
/// # Errors
///
/// Returns an error when `id` is missing or the task does not exist.
pub(crate) async fn get(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("id"))?;

    let task = ig
        .get_task(id)
        .await
        .map_err(|e| ingot_err(&e))?
        .ok_or_else(|| RpcError::new(codes::INTERNAL_ERROR, format!("task not found: {id}")))?;

    Ok(json!({
        "id": task.id,
        "status": task.status,
        "title": task.title,
        "response": task.response,
    }))
}

/// Handles `task.list`.
///
/// # Errors
///
/// Returns an error when the ingot query fails.
pub(crate) async fn list(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let status = params
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let tasks = ig.list_tasks(status).await.map_err(|e| ingot_err(&e))?;
    let out: Vec<Value> = tasks
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "title": t.title,
                "status": t.status,
                "created_at": t.created_at,
                "session_id": t.session_id,
            })
        })
        .collect();
    Ok(Value::Array(out))
}

/// Handles `task.create`.
///
/// # Errors
///
/// Returns an error when `title` is missing or the ingot write fails.
pub(crate) async fn create(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let title = params
        .get("title")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("title"))?
        .to_owned();
    let description = params
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let task = Task {
        id: Uuid::new_v4(),
        title,
        description,
        status: "planned".to_owned(),
        created_at: Timestamp::now(),
        session_id,
        response: None,
    };
    ig.create_task(task.clone())
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "id": task.id, "status": task.status }))
}

/// Handles `task.close`.
///
/// # Errors
///
/// Returns an error when `id` is missing or the status update fails.
pub(crate) async fn close(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let id = params
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("id"))?;
    ig.update_task_status(id, "complete")
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "id": id, "status": "complete" }))
}

/// Handles `task.parallel`: fans a goal out across roles into git worktrees and
/// snapshots warm context into the vault.
///
/// # Errors
///
/// Returns an error when `goal` is missing or a role resume depth is exceeded.
#[allow(clippy::too_many_lines)] // single fan-out pipeline kept inline
pub(crate) async fn parallel(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let pool = state.worktree_pool;
    let vt = state.vault;
    let session_id = params["session_id"].as_str().map(str::to_owned);
    let goal = params["goal"]
        .as_str()
        .ok_or_else(|| missing_param("goal"))?
        .to_owned();
    // Roles may be plain strings or `{name, resume_session_id?}` objects.
    // Both produce the consolidated `smedja_loop::LoopRole` (single source
    // of truth); the fan-out path uses only `name` + `resume_session_id`.
    let loop_roles: Vec<smedja_loop::LoopRole> = params["roles"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| {
            if let Some(name) = v.as_str() {
                Some(smedja_loop::LoopRole::for_parallel(name, None))
            } else if let Some(obj) = v.as_object() {
                obj.get("name").and_then(Value::as_str).map(|name| {
                    smedja_loop::LoopRole::for_parallel(
                        name,
                        obj.get("resume_session_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    )
                })
            } else {
                None
            }
        })
        .collect();

    // Enforce MAX_ROLE_DEPTH: reject any role that tries to resume a session at depth ≥ 4.
    {
        for role in &loop_roles {
            if let Some(ref resume_sid) = role.resume_session_id {
                let depth = ig
                    .list_compaction_checkpoints(resume_sid)
                    .await
                    .unwrap_or_default()
                    .len();
                #[allow(clippy::cast_possible_truncation)]
                if depth as u8 >= smedja_assayer::MAX_ROLE_DEPTH {
                    return Err(RpcError::new(
                        codes::INVALID_PARAMS,
                        format!(
                            "resume depth exceeded for role '{}': max {}",
                            role.name,
                            smedja_assayer::MAX_ROLE_DEPTH,
                        ),
                    ));
                }
            }
        }
    }
    let roles: Vec<String> = loop_roles.iter().map(|r| r.name.clone()).collect();

    // Derive workspace root: prefer session.workspace_root, then env, then ".".
    let workspace_root = if let Some(ref sid) = session_id {
        ig.get_session(sid)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.workspace_root)
            .map_or_else(crate::common::workspace_root, PathBuf::from)
    } else {
        crate::common::workspace_root()
    };

    if !workspace_root.join(".git").exists() {
        tracing::warn!(
            path = %workspace_root.display(),
            "task.parallel workspace does not contain .git",
        );
    }

    let mut p = pool.lock().await;

    // Register all roles first (synchronous — no await).
    let registered: Vec<(String, String)> = roles
        .iter()
        .map(|role| {
            let id = p.register(role, &goal, &workspace_root);
            (role.clone(), id)
        })
        .collect();

    // Create the git worktrees for all pending tasks.
    let started = p.start_worktrees(&workspace_root).await;

    // Warm-context snapshot: write recent checkpoints to vault so parallel
    // agents can retrieve shared context via smedja_vault_search.
    let fan_out_id = Uuid::new_v4().to_string();
    if let Some(ref sid) = session_id {
        const WARM_WINDOW: usize = 5;
        let checkpoints = ig.list_checkpoints(sid).await.unwrap_or_default();
        let recent: Vec<_> = checkpoints.into_iter().rev().take(WARM_WINDOW).collect();
        if !recent.is_empty() {
            let fid = fan_out_id.clone();
            let parent_sid = sid.clone();
            let vt2 = Arc::clone(&vt);
            tokio::task::spawn_blocking(move || {
                let mut guard = vt2.blocking_lock();
                for cp in &recent {
                    let entry = VaultEntry {
                        id: format!("warm:{}:{}", fid, cp.id),
                        embedding: crate::embedder::embed(&cp.messages_json),
                        payload: serde_json::json!({
                            "fan_out_id": fid,
                            "session_id": parent_sid,
                            "turn_n": cp.turn_n,
                        }),
                        namespace: "warm".to_owned(),
                        content: cp.messages_json.clone(),
                        source_file: None,
                        added_by: Some("task.parallel".to_owned()),
                        chunk_index: None,
                        parent_id: None,
                        created_at: 0.0,
                    };
                    let _ = guard.upsert(&entry);
                }
            });
        }
    }

    // Build the per-task response, including worktree_path where available.
    let tasks: Vec<Value> = registered
        .iter()
        .map(|(role, task_id)| {
            let worktree_path = p
                .get(task_id)
                .map(|t| t.worktree_path.to_string_lossy().into_owned())
                .unwrap_or_default();
            json!({
                "role": role,
                "task_id": task_id,
                "worktree_path": worktree_path,
            })
        })
        .collect();

    Ok(json!({
        "goal": goal,
        "tasks": tasks,
        "started": started,
        "fan_out_id": fan_out_id,
        "warm_context_namespace": "warm",
    }))
}

/// Handles `task.cancel`.
///
/// # Errors
///
/// Returns an error when `task_id` is missing.
pub(crate) async fn cancel(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let pool = state.worktree_pool;
    let task_id = params["task_id"]
        .as_str()
        .ok_or_else(|| missing_param("task_id"))?
        .to_owned();
    let found = pool.lock().await.cancel(&task_id);
    Ok(json!({ "task_id": task_id, "cancelled": found }))
}
