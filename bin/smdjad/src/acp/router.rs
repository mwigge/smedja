//! ACP router construction, auth middleware, and health probe.

use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::Json;
use axum::Router;
use serde_json::json;

use super::prompt::{mcp_server_endpoint, submit_prompt};
use super::session::{close_session, create_session, set_mode, set_model};
use super::sse::get_turn_events;
use super::state::AcpState;

/// Builds the ACP router with auth middleware applied to every route.
pub fn build_acp_router(state: AcpState) -> Router {
    Router::new()
        .route("/acp/v1/session/new", post(create_session))
        .route("/acp/v1/session/{id}/prompt", post(submit_prompt))
        .route(
            "/acp/v1/session/{id}/events/{turn_id}",
            get(get_turn_events),
        )
        .route("/acp/v1/session/{id}/model", post(set_model))
        .route("/acp/v1/session/{id}/mode", post(set_mode))
        .route("/acp/v1/session/{id}", delete(close_session))
        // MCP server mode — JSON-RPC 2.0 over the same authenticated listener.
        .route("/mcp", post(mcp_server_endpoint))
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

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};

    use super::super::state::test_state;
    use super::build_acp_router;
    use tower::ServiceExt as _;

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
    async fn mcp_endpoint_rejects_unauthenticated_request() {
        let app = build_acp_router(test_state());
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        // No Authorization header → rejected before any dispatch.
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

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
