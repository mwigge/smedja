//! Turn RPC handlers: `turn.submit`, `turn.subscribe`.

use serde_json::{json, Value};
use smedja_bellows::event::CorrelationCtx;
use smedja_bellows::TurnEvent;
use smedja_ingot::Task;
use smedja_rpc::RpcError;
use smedja_telemetry as tel;
use smedja_types::Timestamp;
use uuid::Uuid;

use std::collections::HashMap;
use std::sync::Arc;

use smedja_ingot::IngotHandle;
use tokio::sync::Mutex;

use crate::cowork::CoworkGate;
use crate::handlers::HandlerState;
use crate::{await_turn_terminal, fragments, ingot_err, missing_param};

/// Resolves the workspace root and cowork gate for `session_id`, then expands any
/// inline context fragments in `content`.
///
/// The workspace is the session's `workspace_root`, falling back to
/// `SMEDJA_WORKSPACE` and then the relative `"."` — matching the orchestrator's
/// per-turn resolution. The cowork gate is supplied only when the session has
/// `cowork_mode` enabled and a gate is registered, so `@shell` is gated exactly
/// as the `bash` tool is. A session lookup failure degrades to no-fragment
/// expansion against the default workspace rather than failing the submit.
async fn expand_submission(
    ig: &IngotHandle,
    gates: &Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    session_id: &str,
    content: String,
    lsp: Option<&smedja_lsp::LspManager>,
) -> String {
    let session = ig.get_session(session_id).await.ok().flatten();

    let workspace = session
        .as_ref()
        .and_then(|s| s.workspace_root.as_deref())
        .map_or_else(crate::common::workspace_root, std::path::PathBuf::from);

    let gate = if session.as_ref().is_some_and(|s| s.cowork_mode) {
        gates.lock().await.get(session_id).cloned()
    } else {
        None
    };

    fragments::expand(&content, &workspace, gate.as_deref(), lsp).await
}

/// Handles `turn.submit`: records a new turn task and publishes `Started`.
///
/// # Errors
///
/// Returns an error when `session_id` or `content` is missing, or the ingot
/// write fails.
pub(crate) async fn submit(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let dispatcher = state.dispatcher;
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("session_id"))?
        .to_owned();
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("content"))?
        .to_owned();

    // Expand inline context fragments (`@file`, `@git`, `@branch`, `@shell`,
    // `@clippy`, `@lsp`) against the session workspace before the prompt is
    // recorded, so the stored and executed prompt is the expanded text.
    let content = expand_submission(
        &ig,
        &state.gates,
        &session_id,
        content,
        Some(&state.lsp_manager),
    )
    .await;

    let task_id = Uuid::new_v4();
    let task = Task {
        id: task_id,
        title: content,
        description: String::new(),
        status: "planned".to_owned(),
        created_at: Timestamp::now(),
        session_id: Some(session_id.clone()),
        response: None,
    };

    ig.create_task(task.clone())
        .await
        .map_err(|e| ingot_err(&e))?;

    // Extract current span IDs for turn start event correlation.
    let (ts_trace_id, ts_span_id) = crate::common::current_span_ids();
    dispatcher.publish(TurnEvent::Started {
        session_id: session_id.clone(),
        turn_id: task_id.to_string(),
        correlation: CorrelationCtx {
            conversation_id: Some(session_id.clone()),
            trace_id: ts_trace_id,
            span_id: ts_span_id,
            parent_span_id: None,
            operation_name: Some(tel::OPERATION_INVOKE_AGENT.to_owned()),
            agent_name: Some("interactive".to_owned()),
            status: None,
        },
    });

    // Also send directly to the worker via a dedicated mpsc channel so the
    // Started event cannot be dropped if the broadcast is temporarily full.
    let _ = state
        .work_tx
        .send((session_id.clone(), task_id.to_string()))
        .await;

    Ok(json!({ "task_id": task_id }))
}

/// Handles `turn.subscribe`: blocks until the named task reaches a terminal
/// status or a 60-second deadline expires.
///
/// # Errors
///
/// Returns an error when `task_id` is missing, the task does not exist, or the
/// wait times out.
pub(crate) async fn subscribe(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let ig = state.ingot;
    let dispatcher = state.dispatcher;
    let task_id = params
        .get("task_id")
        .and_then(Value::as_str)
        .ok_or_else(|| missing_param("task_id"))?
        .to_owned();
    // Event-driven: resolve on the terminal TurnEvent for this turn,
    // bounded by a 60s deadline. No fixed-interval polling.
    await_turn_terminal(
        &ig,
        &dispatcher,
        &task_id,
        std::time::Duration::from_mins(1),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_ingot::{Ingot, Session};
    use smedja_types::Timestamp;
    use uuid::Uuid;

    /// Builds a session rooted at `workspace`, with cowork disabled.
    fn session_at(id: &str, workspace: &std::path::Path) -> Session {
        Session {
            id: Uuid::parse_str(id).unwrap(),
            created_at: Timestamp::from_micros(0),
            updated_at: Timestamp::from_micros(0),
            status: "active".to_owned(),
            task_id: None,
            mode: None,
            title: String::new(),
            cowork_mode: false,
            workspace_root: Some(workspace.display().to_string()),
            model_override: None,
            runner_override: None,
        }
    }

    fn empty_gates() -> Arc<Mutex<HashMap<String, Arc<CoworkGate>>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[tokio::test]
    async fn submit_expands_fragments_before_recording_task() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        tokio::fs::write(ws.join("note.txt"), b"hello from file")
            .await
            .unwrap();

        let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let sid = Uuid::new_v4().to_string();
        ig.create_session(session_at(&sid, &ws)).await.unwrap();

        let expanded = expand_submission(
            &ig,
            &empty_gates(),
            &sid,
            "look @file note.txt".to_owned(),
            None,
        )
        .await;

        assert!(
            expanded.contains("hello from file"),
            "file contents must be injected: {expanded}"
        );
        assert!(
            !expanded.contains("@file note.txt"),
            "raw @file token must not survive: {expanded}"
        );
        assert!(expanded.starts_with("look "), "prose preserved: {expanded}");
    }

    #[tokio::test]
    async fn submit_passes_through_when_no_fragments() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();

        let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let sid = Uuid::new_v4().to_string();
        ig.create_session(session_at(&sid, &ws)).await.unwrap();

        let raw = "plain question with an email foo@bar.com and no fragments";
        let out = expand_submission(&ig, &empty_gates(), &sid, raw.to_owned(), None).await;
        assert_eq!(
            out, raw,
            "fragment-free content must pass through byte-for-byte"
        );
    }
}
