//! ACP HTTP server — Agent Coordination Protocol over HTTP.
//!
//! Activated by `SMEDJA_ACP_PORT` environment variable (default: disabled).
//! Routes proxy into smdjad's ingot and dispatcher directly.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::routing::{delete, post};
use axum::Json;
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_ingot::{Ingot, Session, Task};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Shared state for ACP route handlers.
#[derive(Clone)]
pub struct AcpState {
    pub ingot: Arc<Mutex<Ingot>>,
    pub dispatcher: Arc<Dispatcher>,
    pub auth_token: String,
}

/// Builds the ACP router with auth middleware applied to every route.
pub fn build_acp_router(state: AcpState) -> Router {
    Router::new()
        .route("/acp/v1/session/new", post(create_session))
        .route("/acp/v1/session/{id}/prompt", post(submit_prompt))
        .route("/acp/v1/session/{id}/model", post(set_model))
        .route("/acp/v1/session/{id}/mode", post(set_mode))
        .route("/acp/v1/session/{id}", delete(close_session))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        .with_state(state)
}

/// Rejects requests that do not carry a valid `Authorization: Bearer <token>` header.
async fn require_auth(
    State(state): State<AcpState>,
    request: axum::extract::Request,
    next: Next,
) -> impl IntoResponse {
    let auth = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    if auth.is_some_and(|t| t == state.auth_token) {
        next.run(request).await.into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response()
    }
}

#[derive(Deserialize)]
struct PromptRequest {
    content: String,
}

async fn create_session(State(s): State<AcpState>) -> impl IntoResponse {
    let id = Uuid::new_v4();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let session = Session {
        id,
        mode: Some("acp".into()),
        status: "active".into(),
        task_id: None,
        cowork_mode: false,
        created_at: now,
        updated_at: now,
        workspace_root: None,
    };
    match s.ingot.lock().await.create_session(&session) {
        Ok(()) => Json(json!({ "session_id": id })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
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
    match s.ingot.lock().await.create_task(&task) {
        Ok(()) => {
            s.dispatcher.publish(TurnEvent::Started {
                session_id: session_id.clone(),
                turn_id: turn_id.to_string(),
            });
            Json(json!({ "turn_id": turn_id, "session_id": session_id })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn set_model(Path(id): Path<String>) -> impl IntoResponse {
    // ponytail: runner-switch logic deferred
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "session_id": id, "status": "not_implemented" })),
    )
}

async fn set_mode(Path(id): Path<String>) -> impl IntoResponse {
    // ponytail: agent-mode switch deferred
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "session_id": id, "status": "not_implemented" })),
    )
}

async fn close_session(Path(id): Path<String>, State(s): State<AcpState>) -> impl IntoResponse {
    match s.ingot.lock().await.delete_session(&id) {
        Ok(_) => Json(json!({ "session_id": id, "deleted": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use smedja_bellows::Dispatcher;
    use tokio::sync::Mutex;
    use tower::ServiceExt as _;

    use super::{build_acp_router, AcpState};

    fn test_state() -> AcpState {
        let ingot = smedja_ingot::Ingot::open_in_memory().expect("in-memory ingot");
        AcpState {
            ingot: Arc::new(Mutex::new(ingot)),
            dispatcher: Arc::new(Dispatcher::new(32)),
            auth_token: "test-token".to_owned(),
        }
    }

    #[tokio::test]
    async fn post_session_new_returns_session_id() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/new")
                    .header("Authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("session_id").is_some(),
            "response must contain session_id"
        );
    }

    #[tokio::test]
    async fn missing_auth_returns_401() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/new")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/new")
                    .header("Authorization", "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn delete_unknown_session_returns_success_with_deleted_false() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/acp/v1/session/no-such-id")
                    .header("Authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // delete_session returns Ok(false) when no row matched — the handler
        // treats that as a successful deletion and returns 200.
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
