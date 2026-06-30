//! Loop RPC handlers: `loop.create/status/cancel/list/retire/list_by_status/run/resume`.

use std::sync::Arc;

use serde_json::{json, Value};
use smedja_rpc::{codes, RpcError};
use smedja_types::Timestamp;
use uuid::Uuid;

use crate::handlers::HandlerState;
use crate::{ingot_err, missing_param};

/// Handles `loop.create`.
///
/// # Errors
///
/// Returns an error when `change_name` is missing or the ingot write fails.
pub(crate) async fn create(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let change_name = params["change_name"]
        .as_str()
        .ok_or_else(|| missing_param("change_name"))?
        .to_owned();
    let now = Timestamp::now();
    let rec = smedja_ingot::LoopRecord {
        id: Uuid::new_v4().to_string(),
        change_name,
        status: "planned".to_owned(),
        current_slice: 0,
        attempt: 1,
        created_at: now,
        updated_at: now,
    };
    let loop_id = rec.id.clone();
    ig.create_loop(rec).await.map_err(|e| ingot_err(&e))?;
    Ok(json!({ "loop_id": loop_id }))
}

/// Handles `loop.status`.
///
/// # Errors
///
/// Returns an error when `loop_id` is missing or the loop does not exist.
pub(crate) async fn status(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let loop_id = params["loop_id"]
        .as_str()
        .ok_or_else(|| missing_param("loop_id"))?
        .to_owned();
    let rec = ig
        .get_loop(&loop_id)
        .await
        .map_err(|e| ingot_err(&e))?
        .ok_or_else(|| {
            RpcError::new(codes::INVALID_PARAMS, format!("loop not found: {loop_id}"))
        })?;
    Ok(serde_json::to_value(&rec).unwrap_or(Value::Null))
}

/// Handles `loop.cancel`.
///
/// # Errors
///
/// Returns an error when `loop_id` is missing or the status update fails.
pub(crate) async fn cancel(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let loop_id = params["loop_id"]
        .as_str()
        .ok_or_else(|| missing_param("loop_id"))?
        .to_owned();
    ig.update_loop_status(&loop_id, "cancelled", Timestamp::now())
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "loop_id": loop_id, "status": "cancelled" }))
}

/// Handles `loop.list`.
///
/// # Errors
///
/// Returns an error when `change_name` is missing or the ingot query fails.
pub(crate) async fn list(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let change_name = params["change_name"]
        .as_str()
        .ok_or_else(|| missing_param("change_name"))?
        .to_owned();
    let loops = ig
        .list_loops(&change_name)
        .await
        .map_err(|e| ingot_err(&e))?;
    let loops_json: Vec<Value> = loops
        .into_iter()
        .map(|r| serde_json::to_value(&r).unwrap_or(Value::Null))
        .collect();
    Ok(json!({ "loops": loops_json }))
}

/// Handles `loop.retire`.
///
/// # Errors
///
/// Returns an error when `loop_id` is missing, the loop does not exist, or the
/// loop is not in a terminal (complete/failed) state.
pub(crate) async fn retire(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let loop_id = params["loop_id"]
        .as_str()
        .ok_or_else(|| missing_param("loop_id"))?
        .to_owned();
    let rec = ig
        .get_loop(&loop_id)
        .await
        .map_err(|e| ingot_err(&e))?
        .ok_or_else(|| {
            RpcError::new(codes::INVALID_PARAMS, format!("loop not found: {loop_id}"))
        })?;
    // Only complete or failed loops can be retired.
    if rec.status != "complete" && rec.status != "failed" {
        return Err(RpcError::new(
            codes::INVALID_PARAMS,
            format!(
                "loop is in state '{}'; only complete or failed loops can be retired",
                rec.status
            ),
        ));
    }
    ig.update_loop_status(&loop_id, "retired", Timestamp::now())
        .await
        .map_err(|e| ingot_err(&e))?;
    Ok(json!({ "loop_id": loop_id, "status": "retired" }))
}

/// Handles `loop.list_by_status`.
///
/// # Errors
///
/// Returns an error when the ingot query fails.
pub(crate) async fn list_by_status(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let status = params["status"].as_str().map(str::to_owned);
    let loops = ig
        .list_loops_by_status(status)
        .await
        .map_err(|e| ingot_err(&e))?;
    let loops_json: Vec<Value> = loops
        .into_iter()
        .map(|r| serde_json::to_value(&r).unwrap_or(Value::Null))
        .collect();
    Ok(json!({ "loops": loops_json }))
}

/// Handles `loop.run`: drives the real `smedja-loop` engine for a loop record.
///
/// # Errors
///
/// Returns an error when `loop_id` is missing, the loop does not exist, the loop
/// is retired, or the change name fails the path-traversal guard.
pub(crate) async fn run(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let loop_id = params["loop_id"]
        .as_str()
        .ok_or_else(|| missing_param("loop_id"))?
        .to_owned();

    // Verify the loop record exists before spawning background work.
    let rec = ig
        .get_loop(&loop_id)
        .await
        .map_err(|e| ingot_err(&e))?
        .ok_or_else(|| {
            RpcError::new(codes::INVALID_PARAMS, format!("loop not found: {loop_id}"))
        })?;

    // Retired loops cannot be re-run.
    if rec.status == "retired" {
        return Err(RpcError::new(
            codes::INVALID_PARAMS,
            "loop is retired and cannot be re-run",
        ));
    }

    // Guard against path traversal via change_name.
    if rec.change_name.contains("..") || rec.change_name.contains('/') {
        return Err(RpcError::new(codes::INVALID_PARAMS, "invalid change_name"));
    }

    let workspace_root = crate::common::workspace_root();
    let change_name = rec.change_name.clone();
    let bg_loop_id = loop_id.clone();

    // Spawn the engine-backed runner into the shared task set so it is
    // drained at shutdown; the caller gets an immediate response.
    state.task_set.lock().await.spawn(crate::loop_runner::run(
        ig,
        Arc::clone(&state.dispatcher),
        Arc::clone(&state.gates),
        Arc::clone(&state.provider_pool),
        Arc::clone(&state.assayer),
        Arc::clone(&state.price_table),
        Arc::clone(&state.vault),
        Arc::clone(&state.embedder),
        Arc::clone(&state.provider_sessions),
        Arc::clone(&state.cache_aligners),
        Arc::clone(&state.lsp_manager),
        bg_loop_id,
        change_name,
        workspace_root,
    ));

    Ok(json!({ "loop_id": loop_id, "status": "slicing" }))
}

/// Handles `loop.resume`: re-enters the loop engine from the last checkpoint.
///
/// # Errors
///
/// Returns an error when `loop_id` is missing, the loop does not exist, or the
/// change name fails the path-traversal guard.
pub(crate) async fn resume(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let loop_id = params["loop_id"]
        .as_str()
        .ok_or_else(|| missing_param("loop_id"))?
        .to_owned();

    let rec = ig
        .get_loop(&loop_id)
        .await
        .map_err(|e| ingot_err(&e))?
        .ok_or_else(|| {
            RpcError::new(codes::INVALID_PARAMS, format!("loop not found: {loop_id}"))
        })?;

    if rec.status == "retired" {
        return Err(RpcError::new(
            codes::INVALID_PARAMS,
            "loop is retired and cannot be resumed",
        ));
    }

    if rec.change_name.contains("..") || rec.change_name.contains('/') {
        return Err(RpcError::new(codes::INVALID_PARAMS, "invalid change_name"));
    }

    let workspace_root = crate::common::workspace_root();
    let change_name = rec.change_name.clone();
    let bg_loop_id = loop_id.clone();

    state
        .task_set
        .lock()
        .await
        .spawn(crate::loop_runner::resume(
            ig,
            Arc::clone(&state.dispatcher),
            Arc::clone(&state.gates),
            Arc::clone(&state.provider_pool),
            Arc::clone(&state.assayer),
            Arc::clone(&state.price_table),
            Arc::clone(&state.vault),
            Arc::clone(&state.embedder),
            Arc::clone(&state.provider_sessions),
            Arc::clone(&state.cache_aligners),
            Arc::clone(&state.lsp_manager),
            bg_loop_id,
            change_name,
            workspace_root,
        ));

    Ok(json!({ "loop_id": loop_id, "status": "resuming" }))
}
