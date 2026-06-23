//! In-memory alert queue and webhook HTTP router for `smdjad`.
//!
//! The alert queue is a bounded FIFO that drains up to 50 alerts per
//! `alert_list` tool call.  The webhook endpoint accepts `POST /webhook/alert`
//! in `AlertManager` v2 format and is only active when `SMEDJA_ALERT_WEBHOOK`
//! is set to a non-empty value in the environment.

use std::collections::VecDeque;
use std::sync::OnceLock;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use smedja_ingot::{AuditEvent, IngotHandle};
use tokio::sync::Mutex;
use uuid::Uuid;

// ── Alert type ────────────────────────────────────────────────────────────────

/// A normalised alert entry stored in the in-memory queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alert {
    /// Human-readable alert title derived from `alertname` label.
    pub title: String,
    /// Alert body derived from the `description` annotation.
    pub body: String,
    /// Severity label value (e.g. `"critical"`, `"warning"`).
    pub severity: String,
    /// Source field — the `source` label or the alert status when absent.
    pub source: String,
}

// ── Global queue ──────────────────────────────────────────────────────────────

/// Returns a reference to the global alert queue singleton.
///
/// The queue is initialised on first access.  Use
/// [`alert_queue`] everywhere instead of accessing the static directly.
pub fn alert_queue() -> &'static Mutex<VecDeque<Alert>> {
    static QUEUE: OnceLock<Mutex<VecDeque<Alert>>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(VecDeque::new()))
}

// ── AlertManager payload schema ───────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct AlertManagerPayload {
    pub alerts: Vec<AlertManagerAlert>,
}

#[derive(Deserialize)]
pub(crate) struct AlertManagerAlert {
    /// Flat key-value labels (alertname, severity, source, …).
    pub labels: serde_json::Value,
    /// Annotations such as summary and description.
    pub annotations: serde_json::Value,
    /// Alert lifecycle status: `"firing"` or `"resolved"`.
    pub status: String,
}

// ── Shared HTTP state ─────────────────────────────────────────────────────────

/// State shared between the alert webhook handler and the rest of smdjad.
#[derive(Clone)]
pub struct AlertState {
    /// Shared ingot for writing audit events.
    pub ingot: IngotHandle,
}

// ── Route builder ─────────────────────────────────────────────────────────────

/// Builds the alert webhook router.
///
/// Only mount this router when `SMEDJA_ALERT_WEBHOOK` is non-empty.
pub fn build_alert_router(state: AlertState) -> Router {
    Router::new()
        .route("/webhook/alert", post(webhook_alert))
        .with_state(state)
}

// ── Handlers ──────────────────────────────────────────────────────────────────

async fn webhook_alert(
    axum::extract::State(state): axum::extract::State<AlertState>,
    Json(payload): Json<AlertManagerPayload>,
) -> impl IntoResponse {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    // Phase 1 — normalise AlertManager alerts into `Alert` structs, building
    // the audit events simultaneously (no locking required here).
    let mut normalised: Vec<(Alert, AuditEvent)> = Vec::with_capacity(payload.alerts.len());
    for am_alert in &payload.alerts {
        let title = am_alert.labels["alertname"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned();
        let severity = am_alert.labels["severity"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned();
        let source = am_alert.labels["source"]
            .as_str()
            .unwrap_or(am_alert.status.as_str())
            .to_owned();
        let body = am_alert.annotations["description"]
            .as_str()
            .or_else(|| am_alert.annotations["summary"].as_str())
            .unwrap_or("")
            .to_owned();

        let alert = Alert {
            title: title.clone(),
            body,
            severity,
            source,
        };
        let event = AuditEvent {
            id: Uuid::new_v4(),
            ts: now,
            session_id: "webhook".to_owned(),
            turn_id: None,
            action_type: "alert_received".to_owned(),
            actor: "webhook".to_owned(),
            tool_name: Some("alert_list".to_owned()),
            input_tok: 0,
            output_tok: 0,
            latency_ms: 0,
            traceparent: None,
            tier: None,
            role_id: None,
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            agent_name: None,
            operation_name: None,
            status: None,
            error_kind: None,
            error_count: None,
            tool_call_id: None,
        };
        normalised.push((alert, event));
    }

    // Phase 2 — push all alerts into the queue in one critical section
    // (no await while the queue lock is held).
    {
        let mut queue = alert_queue().lock().await;
        for (alert, _) in &normalised {
            if queue.len() >= 200 {
                queue.pop_front();
            }
            queue.push_back(alert.clone());
        }
        // Lock dropped here — before any await.
    }

    // Phase 3 — write audit events; each call goes through spawn_blocking.
    for (alert, event) in normalised.clone() {
        if let Err(e) = state.ingot.insert_audit_event(event).await {
            tracing::warn!(
                alert = %alert.title,
                error = %e,
                "failed to write alert audit event"
            );
        }
    }

    let accepted = normalised.len();
    tracing::info!(count = accepted, "alert webhook: received alerts");

    (StatusCode::OK, Json(json!({ "accepted": accepted }))).into_response()
}

/// Drains up to `max` alerts from the global alert queue and returns them.
///
/// This is the backing implementation for the `alert_list` tool.
pub async fn drain_alerts(max: usize) -> Vec<Alert> {
    let mut queue = alert_queue().lock().await;
    let n = queue.len().min(max);
    queue.drain(..n).collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use smedja_ingot::{Ingot, IngotHandle};
    use tokio::sync::Mutex;
    use tower::ServiceExt as _;

    use super::{alert_queue, build_alert_router, AlertState};

    // Serialise all queue-touching tests: the alert queue is a process-global
    // singleton; tests that run in parallel would race on its contents.
    // A Tokio Mutex is used here so the guard can be held across `.await` points.
    fn queue_test_lock() -> &'static Mutex<()> {
        static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn test_state() -> AlertState {
        let ingot = Ingot::open_in_memory().expect("in-memory ingot");
        AlertState {
            ingot: IngotHandle::new(ingot),
        }
    }

    #[tokio::test]
    async fn webhook_returns_200_for_valid_payload() {
        let _guard = queue_test_lock().lock().await;
        // Drain stale residue from prior tests.
        alert_queue().lock().await.clear();

        let app = build_alert_router(test_state());
        let body = serde_json::json!({
            "alerts": [
                {
                    "labels": { "alertname": "HighCPU", "severity": "warning" },
                    "annotations": { "description": "CPU is high" },
                    "status": "firing"
                }
            ]
        });
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/webhook/alert")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn webhook_pushes_alert_to_queue() {
        let _guard = queue_test_lock().lock().await;
        alert_queue().lock().await.clear();

        let app = build_alert_router(test_state());
        let body = serde_json::json!({
            "alerts": [
                {
                    "labels": { "alertname": "DiskFull", "severity": "critical", "source": "infra" },
                    "annotations": { "description": "Disk is at 99%" },
                    "status": "firing"
                }
            ]
        });

        app.oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/webhook/alert")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

        let mut q = alert_queue().lock().await;
        assert_eq!(q.len(), 1);
        let alert = q.pop_front().unwrap();
        assert_eq!(alert.title, "DiskFull");
        assert_eq!(alert.severity, "critical");
        assert_eq!(alert.source, "infra");
        assert_eq!(alert.body, "Disk is at 99%");
    }

    #[tokio::test]
    async fn webhook_accepts_multiple_alerts() {
        let _guard = queue_test_lock().lock().await;
        alert_queue().lock().await.clear();

        let app = build_alert_router(test_state());
        let body = serde_json::json!({
            "alerts": [
                {
                    "labels": { "alertname": "AlertA", "severity": "warning" },
                    "annotations": { "description": "desc-a" },
                    "status": "firing"
                },
                {
                    "labels": { "alertname": "AlertB", "severity": "critical" },
                    "annotations": { "summary": "sum-b" },
                    "status": "firing"
                }
            ]
        });

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/webhook/alert")
                    .header("Content-Type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let b = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(json["accepted"], 2);
        assert_eq!(alert_queue().lock().await.len(), 2);
    }

    #[tokio::test]
    async fn webhook_status_used_as_source_when_source_label_absent() {
        let _guard = queue_test_lock().lock().await;
        alert_queue().lock().await.clear();

        let app = build_alert_router(test_state());
        let body = serde_json::json!({
            "alerts": [
                {
                    "labels": { "alertname": "NoSourceLabel", "severity": "info" },
                    "annotations": {},
                    "status": "resolved"
                }
            ]
        });

        app.oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/webhook/alert")
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

        let mut q = alert_queue().lock().await;
        assert_eq!(q[0].source, "resolved");
        q.clear();
    }

    #[tokio::test]
    async fn alert_list_drain_returns_up_to_50() {
        let _guard = queue_test_lock().lock().await;
        {
            let mut q = alert_queue().lock().await;
            q.clear();
            // Seed 60 alerts directly.
            for i in 0u32..60 {
                q.push_back(super::Alert {
                    title: format!("Alert{i}"),
                    body: String::new(),
                    severity: "info".into(),
                    source: "test".into(),
                });
            }
        }
        // Drain up to 50 — mirrors the alert_list tool dispatch logic.
        let drained: Vec<super::Alert> = {
            let mut q = alert_queue().lock().await;
            let n = q.len().min(50);
            q.drain(..n).collect()
        };
        assert_eq!(drained.len(), 50);
        // 10 remain.
        let remaining = alert_queue().lock().await.len();
        assert_eq!(remaining, 10);
    }
}
