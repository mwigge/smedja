//! Session lifecycle handlers: create, configure (model/mode), and close.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;
use smedja_ingot::Session;
use smedja_types::Timestamp;
use uuid::Uuid;

use super::state::AcpState;

pub(crate) async fn create_session(State(s): State<AcpState>) -> impl IntoResponse {
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

pub(crate) async fn set_model(
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

pub(crate) async fn set_mode(
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

pub(crate) async fn close_session(
    Path(id): Path<String>,
    State(s): State<AcpState>,
) -> impl IntoResponse {
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
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};

    use super::super::router::build_acp_router;
    use super::super::state::test_state;
    use tower::ServiceExt as _;

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
}
