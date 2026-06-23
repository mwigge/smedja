//! Turn RPC handlers: `turn.submit`, `turn.subscribe`.

use serde_json::{json, Value};
use smedja_bellows::event::CorrelationCtx;
use smedja_bellows::TurnEvent;
use smedja_ingot::Task;
use smedja_rpc::RpcError;
use smedja_telemetry as tel;
use smedja_types::Timestamp;
use uuid::Uuid;

use crate::handlers::HandlerState;
use crate::{await_turn_terminal, ingot_err, missing_param};

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
