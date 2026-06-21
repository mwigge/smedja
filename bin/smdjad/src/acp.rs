//! ACP HTTP server — Agent Coordination Protocol over HTTP.
//!
//! Activated by `SMEDJA_ACP_PORT` environment variable (default: disabled).
//! Routes proxy into smdjad's ingot and dispatcher directly.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::routing::{delete, post};
use axum::Json;
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use smedja_ingot::{Ingot, Session, Task};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Shared state for ACP route handlers.
#[derive(Clone)]
pub struct AcpState {
    pub ingot: Arc<Mutex<Ingot>>,
}

/// Builds the ACP router.
pub fn build_acp_router(state: AcpState) -> Router {
    Router::new()
        .route("/acp/v1/session/new", post(create_session))
        .route("/acp/v1/session/:id/prompt", post(submit_prompt))
        .route("/acp/v1/session/:id/model", post(set_model))
        .route("/acp/v1/session/:id/mode", post(set_mode))
        .route("/acp/v1/session/:id", delete(close_session))
        .with_state(state)
}

#[derive(Deserialize)]
struct PromptRequest {
    content: String,
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

async fn create_session(State(s): State<AcpState>) -> impl IntoResponse {
    let id = Uuid::new_v4();
    let now = now_secs();
    let session = Session {
        id,
        mode: Some("acp".into()),
        status: "active".into(),
        task_id: None,
        cowork_mode: false,
        created_at: now,
        updated_at: now,
    };
    match s.ingot.lock().await.create_session(&session) {
        Ok(()) => Json(json!({ "session_id": id })).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn submit_prompt(
    Path(id): Path<String>,
    State(s): State<AcpState>,
    Json(body): Json<PromptRequest>,
) -> impl IntoResponse {
    // ponytail: full SSE streaming deferred; return turn_id for polling
    let turn_id = Uuid::new_v4();
    let now = now_secs();
    let task = Task {
        id: turn_id,
        session_id: Some(id.clone()),
        title: body.content,
        description: String::new(),
        status: "queued".into(),
        response: None,
        created_at: now,
    };
    match s.ingot.lock().await.create_task(&task) {
        Ok(()) => Json(json!({ "turn_id": turn_id, "session_id": id })).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn set_model(Path(id): Path<String>) -> impl IntoResponse {
    // ponytail: runner-switch logic deferred
    Json(json!({ "session_id": id, "status": "not_implemented" }))
}

async fn set_mode(Path(id): Path<String>) -> impl IntoResponse {
    // ponytail: agent-mode switch deferred
    Json(json!({ "session_id": id, "status": "not_implemented" }))
}

async fn close_session(Path(id): Path<String>, State(s): State<AcpState>) -> impl IntoResponse {
    match s.ingot.lock().await.delete_session(&id) {
        Ok(_) => Json(json!({ "session_id": id, "deleted": true })).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
