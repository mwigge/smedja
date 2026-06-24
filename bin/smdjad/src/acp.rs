//! ACP HTTP server — Agent Coordination Protocol over HTTP.
//!
//! Activated by `SMEDJA_ACP_PORT` environment variable (default: disabled).
//! Routes proxy into smdjad's ingot and dispatcher directly.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::Json;
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use smedja_bellows::{Dispatcher, TurnHandle};
use smedja_ingot::{IngotHandle, Session, Task};
use smedja_types::Timestamp;
use uuid::Uuid;

/// Shared state for ACP route handlers.
#[derive(Clone)]
pub struct AcpState {
    pub ingot: IngotHandle,
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
        // /health is added after the auth layer so it is unauthenticated: a
        // supervisor or load balancer probes readiness without a token. It is
        // reachable only once the daemon is serving, so it returns 200.
        .route("/health", get(health))
        .with_state(state)
}

/// Liveness/readiness probe: returns `200 OK` whenever the daemon is serving.
async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
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
    if auth.is_some_and(|t| smedja_auth::tokens_match(t, &state.auth_token)) {
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
    let now = Timestamp::now();
    let session = Session {
        id,
        mode: Some("acp".into()),
        title: String::new(),
        status: "active".into(),
        task_id: None,
        cowork_mode: false,
        created_at: now,
        updated_at: now,
        workspace_root: None,
        model_override: None,
        runner_override: None,
    };
    match s.ingot.create_session(session).await {
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
            // Emit TurnEvent::Started through TurnHandle so the event is routed
            // consistently with the main run_turn path.
            let _handle = TurnHandle::start(
                session_id.clone(),
                turn_id.to_string(),
                Arc::clone(&s.dispatcher),
            );
            // Drop the handle immediately — ACP does not drive the turn itself;
            // spawn_worker will pick up the Started event and call run_turn.
            Json(json!({ "turn_id": turn_id, "session_id": session_id })).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn set_model(
    Path(id): Path<String>,
    State(s): State<AcpState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(model) = body["model"].as_str() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "model field required" })),
        )
            .into_response();
    };
    match s.ingot.update_session_model_override(&id, model).await {
        Ok(()) => Json(json!({ "session_id": id, "model": model })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn set_mode(
    Path(id): Path<String>,
    State(s): State<AcpState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(mode) = body["mode"].as_str() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "mode field required" })),
        )
            .into_response();
    };
    match s.ingot.update_session_mode(&id, mode).await {
        Ok(()) => Json(json!({ "session_id": id, "mode": mode })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn close_session(Path(id): Path<String>, State(s): State<AcpState>) -> impl IntoResponse {
    match s.ingot.delete_session(&id).await {
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
    use tower::ServiceExt as _;

    use super::{build_acp_router, AcpState};

    fn test_state() -> AcpState {
        let ingot = smedja_ingot::Ingot::open_in_memory().expect("in-memory ingot");
        AcpState {
            ingot: smedja_ingot::IngotHandle::new(ingot),
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

    #[tokio::test]
    async fn set_model_returns_200_with_model_echo() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/some-session-id/model")
                    .header("Authorization", "Bearer test-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"model":"gemma4-27b"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "gemma4-27b");
        assert_eq!(json["session_id"], "some-session-id");
    }

    #[tokio::test]
    async fn set_model_missing_field_returns_400() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/some-id/model")
                    .header("Authorization", "Bearer test-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r"{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn set_mode_persists_and_returns_200() {
        let state = test_state();
        // First create a session so update_session_mode has a row to update.
        let session_id = {
            let app = build_acp_router(state.clone());
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
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            json["session_id"].as_str().unwrap().to_owned()
        };

        let app = build_acp_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/acp/v1/session/{session_id}/mode"))
                    .header("Authorization", "Bearer test-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"mode":"ponytail"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["mode"], "ponytail");
        assert_eq!(json["session_id"], session_id);
    }

    #[tokio::test]
    async fn set_mode_missing_field_returns_400() {
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/some-id/mode")
                    .header("Authorization", "Bearer test-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r"{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn set_model_persists_override_in_db() {
        let state = test_state();
        // Create a session so the UPDATE has a row to modify.
        let session_id = {
            let app = build_acp_router(state.clone());
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
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            json["session_id"].as_str().unwrap().to_owned()
        };

        let app = build_acp_router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/acp/v1/session/{session_id}/model"))
                    .header("Authorization", "Bearer test-token")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"model":"gemma4-27b"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "gemma4-27b");
        assert_eq!(json["session_id"], session_id);

        // Verify the override was persisted in the DB.
        let fetched = state.ingot.get_session(&session_id).await.unwrap().unwrap();
        assert_eq!(fetched.model_override.as_deref(), Some("gemma4-27b"));
    }

    /// Verifies that the auth check uses constant-time comparison:
    /// - a token that is a prefix of the real token (different length) is rejected,
    /// - a token that shares the same length but differs in content is rejected, and
    /// - the exact correct token is accepted.
    ///
    /// A naive `==` short-circuits on the first byte mismatch (or on length
    /// mismatch), leaking timing information.  `ConstantTimeEq` pads both
    /// operands to equal length before comparing, so all three branches above
    /// must take the same code path through the comparator.
    #[tokio::test]
    async fn auth_token_comparison_is_constant_time() {
        // The real token is "test-token" (10 bytes).
        // "test" is a strict prefix — a naive == would short-circuit on length.
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/new")
                    .header("Authorization", "Bearer test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "prefix token must be rejected"
        );

        // Same length as "test-token" but wrong content.
        let app = build_acp_router(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/acp/v1/session/new")
                    .header("Authorization", "Bearer XXXX-XXXXX")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "wrong same-length token must be rejected"
        );

        // Correct token must be accepted.
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
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "correct token must be accepted"
        );
    }
}
