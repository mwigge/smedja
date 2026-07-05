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
    create_with(&state.ingot, &params).await
}

/// Core of `task.create`, parameterised on the ingot handle so it is testable
/// without constructing a full [`HandlerState`].
async fn create_with(ig: &smedja_ingot::IngotHandle, params: &Value) -> Result<Value, RpcError> {
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
    let embedder = state.embedder;
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
            let model_id = embedder.model_id().to_owned();
            let dim = embedder.dim();
            // Embed every checkpoint on the async path before the blocking write.
            let mut warm_rows = Vec::with_capacity(recent.len());
            for cp in &recent {
                warm_rows.push((embedder.embed_query(&cp.messages_json).await, cp.clone()));
            }
            tokio::task::spawn_blocking(move || {
                let mut guard = vt2.blocking_lock();
                for (embedding, cp) in warm_rows {
                    let entry = VaultEntry {
                        id: format!("warm:{}:{}", fid, cp.id),
                        embedding,
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
                        embedder_model_id: model_id.clone(),
                        dim,
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

    // Live shared block: seed a durable, concurrently-editable coordination block
    // keyed by `fan_out_id`. Unlike the warm snapshot above (a stale, pull-only
    // copy of the parent's last checkpoints), this block is one the fan-out roles
    // both READ and APPEND to during the run via `task.block_append`/`block_read`
    // (or the MCP `memory_*` tools). The parent seeds the goal as the canonical
    // value; the warm-snapshot pull path is left intact as a fallback.
    {
        let vt2 = Arc::clone(&vt);
        let block_id = fan_out_id.clone();
        let goal_seed = goal.clone();
        let embedding = embedder.embed_query(&goal_seed).await;
        let model_id = embedder.model_id().to_owned();
        let dim = embedder.dim();
        tokio::task::spawn_blocking(move || {
            let mut guard = vt2.blocking_lock();
            if let Err(e) = guard.block_rewrite(
                &block_id,
                "task.parallel",
                &goal_seed,
                embedding,
                &model_id,
                dim,
            ) {
                tracing::warn!(error = %e, "task.parallel: failed to seed shared block");
            }
        });
    }

    Ok(json!({
        "goal": goal,
        "tasks": tasks,
        "started": started,
        "fan_out_id": fan_out_id,
        "warm_context_namespace": "warm",
        "shared_block_id": fan_out_id,
        "shared_block_namespace": smedja_vault::SHARED_BLOCK_NAMESPACE,
    }))
}

/// Handles `task.block_append`: appends a segment to a shared coordination block.
///
/// Additive and concurrency-safe — every call adds a distinct segment, so
/// parallel roles appending to the same `block_id` never clobber one another.
///
/// # Errors
///
/// Returns an error when `block_id` or `content` is missing, or the vault write
/// fails.
pub(crate) async fn block_append(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    block_write(state, params, false).await
}

/// Handles `task.block_rewrite`: replaces the block's single canonical segment
/// (owner-maintained, last-writer-wins). Distinct from the append log.
///
/// # Errors
///
/// Returns an error when `block_id` or `content` is missing, or the vault write
/// fails.
pub(crate) async fn block_rewrite(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    block_write(state, params, true).await
}

/// Shared body for `task.block_append`/`task.block_rewrite`.
async fn block_write(
    state: HandlerState,
    params: Value,
    canonical: bool,
) -> Result<Value, RpcError> {
    let vt = state.vault;
    let embedder = state.embedder;
    let block_id = params
        .get("block_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("block_id"))?
        .to_owned();
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("content"))?
        .to_owned();
    let author = params
        .get("author")
        .and_then(Value::as_str)
        .unwrap_or("agent")
        .to_owned();

    let embedding = embedder.embed_query(&content).await;
    let model_id = embedder.model_id().to_owned();
    let dim = embedder.dim();

    let seg = tokio::task::spawn_blocking(move || {
        let mut guard = vt.blocking_lock();
        if canonical {
            guard.block_rewrite(&block_id, &author, &content, embedding, &model_id, dim)
        } else {
            guard.block_append(&block_id, &author, &content, embedding, &model_id, dim)
        }
    })
    .await
    .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("block write panicked: {e}")))?
    .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;

    Ok(json!({
        "id": seg.id,
        "block_id": seg.block_id,
        "kind": seg.kind.as_str(),
    }))
}

/// Handles `task.block_read`: returns every segment of a shared block, oldest
/// first, so a late-joining role sees the whole coordination log.
///
/// # Errors
///
/// Returns an error when `block_id` is missing or the vault read fails.
pub(crate) async fn block_read(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let vt = state.vault;
    let block_id = params
        .get("block_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("block_id"))?
        .to_owned();

    let segs = tokio::task::spawn_blocking(move || {
        let guard = vt.blocking_lock();
        guard.block_read(&block_id)
    })
    .await
    .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("block read panicked: {e}")))?
    .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;

    let segments: Vec<Value> = segs
        .into_iter()
        .map(|s| {
            json!({
                "id": s.id,
                "author": s.author,
                "content": s.content,
                "kind": s.kind.as_str(),
                "created_at": s.created_at,
            })
        })
        .collect();
    Ok(json!({ "segments": segments }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_ingot::{Ingot, IngotHandle};

    fn handle() -> IngotHandle {
        IngotHandle::new(Ingot::open_in_memory().unwrap())
    }

    // ── task.create ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_returns_id_and_planned_status() {
        let ig = handle();
        let params = json!({ "title": "Fix the bug" });
        let resp = create_with(&ig, &params).await.unwrap();
        assert_eq!(resp["status"], "planned");
        assert!(resp["id"].as_str().is_some(), "id must be present");
    }

    #[tokio::test]
    async fn create_task_is_persisted_and_retrievable() {
        let ig = handle();
        let params = json!({ "title": "Ship it", "description": "desc" });
        let resp = create_with(&ig, &params).await.unwrap();
        let id = resp["id"].as_str().unwrap();

        let task = ig.get_task(id).await.unwrap().unwrap();
        assert_eq!(task.title, "Ship it");
        assert_eq!(task.description, "desc");
        assert_eq!(task.status, "planned");
    }

    #[tokio::test]
    async fn create_missing_title_returns_invalid_params() {
        let ig = handle();
        let params = json!({ "description": "no title" });
        let err = create_with(&ig, &params).await.unwrap_err();
        assert_eq!(err.code, smedja_rpc::codes::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn create_with_optional_session_id_stored() {
        let ig = handle();
        let params = json!({ "title": "t", "session_id": "sess-abc" });
        let resp = create_with(&ig, &params).await.unwrap();
        let id = resp["id"].as_str().unwrap();

        let task = ig.get_task(id).await.unwrap().unwrap();
        assert_eq!(task.session_id.as_deref(), Some("sess-abc"));
    }
}
