//! Read-only session queries: `session.list/search/get/delete/token_usage/`
//! `context/history`.

use std::sync::Arc;

use serde_json::{json, Value};
use smedja_rpc::{codes, RpcError};

use crate::handlers::HandlerState;
use crate::{ingot_err, missing_param};

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
    let start = sessions.len().saturating_sub(10);
    let out: Vec<Value> = sessions[start..]
        .iter()
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

/// Handles `session.search`: returns sessions whose title or `workspace_root` matches `query`.
///
/// # Errors
///
/// Returns an error when `query` is missing or the ingot read fails.
pub(crate) async fn search(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let query = params["query"]
        .as_str()
        .ok_or_else(|| missing_param("query"))?
        .to_owned();
    search_with(&state.ingot, &query).await
}

async fn search_with(ig: &smedja_ingot::IngotHandle, query: &str) -> Result<Value, RpcError> {
    let sessions = ig.search_sessions(query).await.map_err(|e| ingot_err(&e))?;
    let out: Vec<Value> = sessions
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "title": s.title,
                "mode": s.mode,
                "workspace_root": s.workspace_root,
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
    use smedja_ingot::{Ingot, IngotHandle, Session};
    use smedja_types::Timestamp;
    use uuid::Uuid;

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

    // ── session.search ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn search_matches_title_substring() {
        let ig = handle();
        let id = Uuid::new_v4();
        ig.create_session(sample_session(id, "rust memory pressure"))
            .await
            .unwrap();
        let resp = search_with(&ig, "memory").await.unwrap();
        let arr = resp.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"].as_str().unwrap(), id.to_string());
    }

    #[tokio::test]
    async fn search_returns_empty_for_no_match() {
        let ig = handle();
        ig.create_session(sample_session(Uuid::new_v4(), "alpha"))
            .await
            .unwrap();
        let resp = search_with(&ig, "zzznomatch").await.unwrap();
        assert_eq!(resp.as_array().unwrap().len(), 0);
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

    #[tokio::test]
    async fn list_caps_at_ten_most_recent_sessions() {
        let ig = handle();
        for i in 0u8..15 {
            ig.create_session(sample_session(Uuid::new_v4(), &format!("s{i}")))
                .await
                .unwrap();
        }
        let resp = list_with(&ig).await.unwrap();
        let arr = resp.as_array().unwrap();
        assert_eq!(arr.len(), 10, "must return at most 10 sessions");
        // The last 10 created are s5..s14; the first 5 (s0..s4) are dropped.
        let titles: Vec<&str> = arr.iter().map(|v| v["title"].as_str().unwrap()).collect();
        assert!(
            titles.contains(&"s14"),
            "most recent session must be present"
        );
        assert!(!titles.contains(&"s4"), "oldest sessions must be dropped");
    }
}
