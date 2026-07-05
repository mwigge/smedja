//! Turn subscription (wait-for-terminal) and the free-function turn executor.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use smedja_assayer::Assayer;
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_ingot::IngotHandle;
use smedja_rpc::{codes, RpcError};
use smedja_vault::Vault;
use tokio::sync::Mutex;

use crate::cowork::CoworkGate;
use crate::price_table::PriceTable;
use crate::provider_pool::ProviderPool;
use crate::{embedder_port, handlers, ingot_err, orchestrator};

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

/// Executes a single turn: loads the task, calls the LLM, handles tool calls,
/// stores the final response.
#[allow(clippy::too_many_arguments)] // forwarded directly to TurnOrchestrator
pub(crate) async fn run_turn(
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    session_id: String,
    turn_id: String,
    gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    assayer: Arc<Assayer>,
    price_table: Arc<PriceTable>,
    vault: Arc<Mutex<Vault>>,
    embedder: Arc<dyn embedder_port::Embedder>,
    provider_sessions: orchestrator::ProviderSessions,
    cache_aligners: orchestrator::CacheAligners,
    turn_registry: handlers::TurnRegistry,
    active_change: Option<Arc<str>>,
    lsp_manager: Arc<smedja_lsp::LspManager>,
) {
    // Deregister-on-drop: removes this turn's abort handle from the registry
    // whether the turn completes normally *or* is aborted by `turn.cancel`
    // (aborting drops this future, which runs the guard's destructor). This is
    // race-free vs. the worker's insert — the guard only removes on drop, which
    // can only happen after the turn has started running.
    struct Deregister {
        registry: handlers::TurnRegistry,
        turn_id: String,
    }
    impl Drop for Deregister {
        fn drop(&mut self) {
            if let Ok(mut reg) = self.registry.lock() {
                reg.remove(&self.turn_id);
            }
        }
    }
    let _dereg = Deregister {
        registry: turn_registry,
        turn_id: turn_id.clone(),
    };

    orchestrator::TurnOrchestrator::new(
        ingot,
        dispatcher,
        gates,
        pool,
        assayer,
        price_table,
        vault,
        embedder,
        provider_sessions,
        cache_aligners,
        active_change.as_deref().map(str::to_owned),
        lsp_manager,
    )
    .run(session_id, turn_id)
    .await;
}
