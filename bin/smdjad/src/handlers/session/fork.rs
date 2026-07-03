//! `session.fork` handler: forks a session at the latest (or a chosen) turn.

use serde_json::{json, Value};
use smedja_ingot::{Checkpoint, Session};
use smedja_rpc::{codes, RpcError};
use smedja_types::Timestamp;
use uuid::Uuid;

use crate::handlers::HandlerState;
use crate::{ingot_err, missing_param};

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

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_ingot::{Ingot, IngotHandle};

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
