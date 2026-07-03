//! Event-driven wait for a turn to reach a terminal state.
//!
//! [`await_turn_terminal`] backs the `turn.subscribe` RPC: it blocks until a turn
//! completes or fails (or a deadline elapses), driven by dispatcher events with a
//! direct state read as a lag/absence fallback.

use serde_json::{json, Value};
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_ingot::IngotHandle;
use smedja_rpc::{codes, RpcError};

use crate::paths::ingot_err;

/// Builds the `turn.subscribe` response envelope from a task's current state.
///
/// Returns `Err` when the task does not exist, `Ok(Some(env))` when the task has
/// reached a terminal state (`complete` / `failed`), and `Ok(None)` when it is
/// still in progress.
async fn terminal_envelope(ingot: &IngotHandle, task_id: &str) -> Result<Option<Value>, RpcError> {
    match ingot.get_task(task_id).await.map_err(|e| ingot_err(&e))? {
        None => Err(RpcError::new(
            codes::INTERNAL_ERROR,
            format!("task not found: {task_id}"),
        )),
        Some(t) if t.status == "complete" => {
            // Best-effort token counts from the latest snapshot for the session.
            let (input_tok, output_tok) = if let Some(ref sid) = t.session_id {
                ingot
                    .session_token_snapshots(sid)
                    .await
                    .map_or((0i64, 0i64), |snaps| {
                        snaps
                            .last()
                            .map_or((0i64, 0i64), |s| (s.input_tok, s.output_tok))
                    })
            } else {
                (0i64, 0i64)
            };
            Ok(Some(json!({
                "done": true,
                "response": t.response.unwrap_or_default(),
                "input_tok": input_tok,
                "output_tok": output_tok,
            })))
        }
        Some(t) if t.status == "failed" => Ok(Some(json!({
            "done": true,
            "error": t.response.unwrap_or_else(|| "turn failed".into()),
        }))),
        Some(_) => Ok(None),
    }
}

/// Waits for `task_id` to reach a terminal state and returns the
/// `turn.subscribe` response envelope.
///
/// Subscribes to the dispatcher *before* the initial state read so no terminal
/// event published after subscription is missed; on subscriber lag it falls back
/// to a direct state read; the wait is bounded by `timeout`.
#[tracing::instrument(skip(ingot, dispatcher), fields(turn_id = %task_id))]
pub(crate) async fn await_turn_terminal(
    ingot: &IngotHandle,
    dispatcher: &Dispatcher,
    task_id: &str,
    timeout: std::time::Duration,
) -> Result<Value, RpcError> {
    use tokio::sync::broadcast::error::RecvError;

    let mut rx = dispatcher.subscribe();

    // Resolve immediately if the task is already terminal (or absent).
    if let Some(env) = terminal_envelope(ingot, task_id).await? {
        return Ok(env);
    }

    let wait = async {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let event_turn = match &ev {
                        TurnEvent::Completed { turn_id, .. }
                        | TurnEvent::Failed { turn_id, .. } => Some(turn_id.as_str()),
                        _ => None,
                    };
                    if event_turn == Some(task_id) {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // A burst dropped events: check state directly so the terminal
                    // signal cannot be silently lost.
                    if let Ok(Some(t)) = ingot.get_task(task_id).await {
                        if t.status == "complete" || t.status == "failed" {
                            return;
                        }
                    }
                }
                Err(RecvError::Closed) => return,
            }
        }
    };

    tokio::time::timeout(timeout, wait)
        .await
        .map_err(|_| RpcError::new(codes::TIMEOUT, "turn.subscribe timed out after 60s"))?;

    // Build the envelope from the now-terminal state.
    match terminal_envelope(ingot, task_id).await? {
        Some(env) => Ok(env),
        None => Err(RpcError::new(
            codes::TIMEOUT,
            "turn ended without a terminal status",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use smedja_bellows::event::CorrelationCtx;
    use smedja_bellows::{Dispatcher, TurnEvent};
    use smedja_ingot::{Ingot, IngotHandle, Task};
    use smedja_types::Timestamp;

    fn task(id: uuid::Uuid, status: &str, response: Option<&str>) -> Task {
        Task {
            id,
            title: "t".to_owned(),
            description: String::new(),
            status: status.to_owned(),
            created_at: Timestamp::from_micros(0),
            session_id: None,
            response: response.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn subscribe_not_found_errors() {
        let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let dispatcher = Dispatcher::new(16);
        let r = super::await_turn_terminal(
            &ig,
            &dispatcher,
            "missing",
            std::time::Duration::from_millis(50),
        )
        .await;
        assert!(r.is_err());
        assert!(r.unwrap_err().message.contains("task not found"));
    }

    #[tokio::test]
    async fn subscribe_already_complete_returns_envelope() {
        let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let dispatcher = Dispatcher::new(16);
        let id = uuid::Uuid::new_v4();
        ig.create_task(task(id, "complete", None)).await.unwrap();
        let env = super::await_turn_terminal(
            &ig,
            &dispatcher,
            &id.to_string(),
            std::time::Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert_eq!(env["done"], true);
        assert!(
            env.get("response").is_some(),
            "complete envelope carries a response field"
        );
    }

    #[tokio::test]
    async fn subscribe_already_failed_returns_error_envelope() {
        let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let dispatcher = Dispatcher::new(16);
        let id = uuid::Uuid::new_v4();
        ig.create_task(task(id, "failed", None)).await.unwrap();
        let env = super::await_turn_terminal(
            &ig,
            &dispatcher,
            &id.to_string(),
            std::time::Duration::from_millis(50),
        )
        .await
        .unwrap();
        assert_eq!(env["done"], true);
        assert_eq!(env["error"], "turn failed");
    }

    #[tokio::test]
    async fn subscribe_times_out_for_in_progress_with_no_event() {
        let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let dispatcher = Dispatcher::new(16);
        let id = uuid::Uuid::new_v4();
        ig.create_task(task(id, "planned", None)).await.unwrap();
        let r = super::await_turn_terminal(
            &ig,
            &dispatcher,
            &id.to_string(),
            std::time::Duration::from_millis(100),
        )
        .await;
        assert!(r.is_err());
        assert_eq!(r.unwrap_err().code, super::codes::TIMEOUT);
    }

    #[tokio::test]
    async fn subscribe_resolves_on_completed_event() {
        let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let dispatcher = Arc::new(Dispatcher::new(16));
        let id = uuid::Uuid::new_v4();
        let id_str = id.to_string();
        ig.create_task(task(id, "planned", None)).await.unwrap();

        // After a short delay, mark complete and publish the terminal event.
        let ig2 = ig.clone();
        let id2 = id_str.clone();
        let dispatcher2 = Arc::clone(&dispatcher);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            ig2.set_task_response(&id2, "done-now").await.unwrap();
            dispatcher2.publish(TurnEvent::Completed {
                session_id: "s".to_owned(),
                turn_id: id2.clone(),
                output_tokens: 0,
                input_tokens: Some(0),
                traceparent: None,
                correlation: CorrelationCtx {
                    status: Some("ok".to_owned()),
                    ..CorrelationCtx::default()
                },
            });
        });

        let env = super::await_turn_terminal(
            &ig,
            &dispatcher,
            &id_str,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(env["done"], true);
        assert_eq!(env["response"], "done-now");
    }

    #[tokio::test]
    async fn joinset_reaps_completed_tasks() {
        // A JoinSet drains finished tasks via try_join_next, so it tracks only
        // in-flight work rather than retaining every handle forever.
        let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        for _ in 0..5 {
            set.spawn(async {});
        }
        // Let them finish.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let mut reaped = 0;
        while set.try_join_next().is_some() {
            reaped += 1;
        }
        assert_eq!(reaped, 5);
        assert!(set.is_empty(), "set must be empty after reaping");
    }
}
