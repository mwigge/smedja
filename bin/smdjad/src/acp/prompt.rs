//! Prompt submission and MCP server-mode endpoint handlers.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use smedja_bellows::TurnHandle;
use smedja_ingot::Task;
use smedja_types::Timestamp;
use uuid::Uuid;

use super::sse::build_turn_sse;
use super::state::AcpState;

#[derive(Deserialize)]
pub(crate) struct PromptRequest {
    content: String,
}

/// MCP server-mode endpoint: parses a JSON-RPC 2.0 request and dispatches it to
/// the read-safe tool handler. Reached only after `require_auth` succeeds, so
/// unauthenticated requests are rejected before any tool dispatch.
pub(crate) async fn mcp_server_endpoint(
    State(s): State<AcpState>,
    Json(request): Json<smedja_rpc::Request>,
) -> impl IntoResponse {
    let response =
        crate::mcp_server::handle_request(&request, &s.workspace, &s.ingot, &s.vault, &s.embedder)
            .await;
    Json(response)
}

pub(crate) async fn submit_prompt(
    Path(id): Path<String>,
    State(s): State<AcpState>,
    Json(body): Json<PromptRequest>,
) -> impl IntoResponse {
    let turn_id = Uuid::new_v4();
    let now = Timestamp::now();
    let session_id = id.clone();
    let task = Task {
        id: turn_id,
        session_id: Some(id.clone()),
        title: body.content,
        description: String::new(),
        status: "queued".into(),
        response: None,
        created_at: now,
    };
    match s.ingot.create_task(task).await {
        Ok(()) => {
            // Subscribe BEFORE starting the TurnHandle so the Started event this
            // handle publishes is observed by the SSE stream.
            let receiver = s.dispatcher.subscribe();
            // Emit TurnEvent::Started through TurnHandle so the event is routed
            // consistently with the main run_turn path. Drop the handle
            // immediately — ACP does not drive the turn itself; spawn_worker
            // picks up the Started event and calls run_turn. The turn remains
            // recorded as a Task, so polling clients can still read the result.
            let _handle = TurnHandle::start(
                session_id.clone(),
                turn_id.to_string(),
                Arc::clone(&s.dispatcher),
            );
            build_turn_sse(
                receiver,
                turn_id.to_string(),
                std::collections::VecDeque::new(),
                s.replay,
                s.next_seq,
            )
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};

    use super::super::router::build_acp_router;
    use super::super::state::test_state;
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn submit_prompt_records_task_for_polling_clients() {
        let state = test_state();
        let session_id = "sse-session".to_owned();

        // Issue submit_prompt; the SSE response will hang until terminal, so we
        // race it against a short timeout and then assert the task exists.
        let app = build_acp_router(state.clone());
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/acp/v1/session/{session_id}/prompt"))
            .header("Authorization", "Bearer test-token")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"content":"do the thing"}"#))
            .unwrap();
        // The handler creates the task synchronously before returning the
        // stream, so a short wait on the oneshot is enough to reach that point.
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), app.oneshot(req)).await;

        // The turn must be recorded as a queued Task so polling still works.
        let tasks = state
            .ingot
            .list_tasks(Some("queued".to_owned()))
            .await
            .expect("list_tasks must succeed");
        assert!(
            tasks
                .iter()
                .any(|t| t.session_id.as_deref() == Some(&session_id)),
            "submit_prompt must record a queued task for the session"
        );
    }

    #[tokio::test]
    async fn mcp_endpoint_lists_tools_when_authenticated() {
        let app = build_acp_router(test_state());
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Authorization", "Bearer test-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            json["result"]["tools"].as_array().is_some(),
            "authenticated tools/list must return a tool array; got: {json}"
        );
    }
}
