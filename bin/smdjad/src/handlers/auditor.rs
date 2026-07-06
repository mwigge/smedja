//! Repo/PR/branch auditor: the `audit.run` RPC handler and its supporting
//! read-only exploration loop.
//!
//! The auditor runs the read-only Review role over a selected scope, exploring
//! the workspace with only `graph_query`, `read_file`, and `list_files`,
//! aggregating the model's output into structured [`AuditFinding`]s. Findings
//! are de-duplicated, persisted as `smedja-ingot` `AuditEvent`s, and rendered to
//! a deterministic markdown report.
//!
//! The loop is genuinely read-only by two independent guarantees: it only ever
//! offers the read-only tool allowlist (any other tool call is rejected and fed
//! back as an error observation), and the session runs in `"review"` mode so the
//! existing `role_allows_write_bash` gate denies write-arity bash. The auditor
//! never constructs a `write_file`/`edit_file` dispatch.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smedja_adapter::types::Message as AdapterMessage;
use smedja_adapter::CallOptions;
use smedja_assayer::{Runner, Tier};
use smedja_bellows::event::CorrelationCtx;
use smedja_bellows::Dispatcher;
use smedja_ingot::{AuditEvent, IngotHandle, Session};
use smedja_rpc::{codes, RpcError};
use smedja_types::Timestamp;
use smedja_vault::Vault;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::executor::execute_tool;
use crate::handlers::HandlerState;
use crate::provider_pool::ProviderPool;

mod audit_loop;
mod findings;
mod persist;
mod scope;

pub(crate) use audit_loop::*;
pub(crate) use findings::*;
pub(crate) use persist::*;
pub(crate) use scope::*;

// ── audit.run handler ────────────────────────────────────────────────────────

/// Handles `audit.run`: resolve scope → seed → loop → dedup → persist → render.
///
/// Params: `{ workspace?, path?, branch?, head?, pr?, diff?, report?, format?,
/// max_iterations? }`.
/// Response: `{ findings, counts, report | report_path }`.
///
/// # Errors
///
/// Returns an [`RpcError`] when scope seeding fails (e.g. a `git` error or an
/// unresolvable pull-request reference) or persistence fails.
pub(crate) async fn run(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let workspace = resolve_workspace(&params);
    let scope = resolve_scope(&params);
    let seed = build_seed(
        &scope,
        &workspace,
        &state.ingot,
        &state.vault,
        &state.embedder,
    )
    .await?;

    // A read-only review-mode session is the second read-only guarantee.
    let session_id = Uuid::new_v4();
    let now = Timestamp::now();
    let session = Session {
        id: session_id,
        created_at: now,
        updated_at: now,
        status: "active".to_owned(),
        task_id: None,
        mode: Some("review".to_owned()),
        title: "audit".to_owned(),
        cowork_mode: false,
        workspace_root: Some(workspace.display().to_string()),
        model_override: None,
        runner_override: None,
    };
    state
        .ingot
        .create_session(session.clone())
        .await
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;

    let budget = LoopBudget {
        max_iterations: params
            .get("max_iterations")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(DEFAULT_MAX_ITERATIONS),
        token_budget: params
            .get("token_budget")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TOKEN_BUDGET),
    };

    let runner = ProviderReviewTurn {
        pool: Arc::clone(&state.provider_pool),
        dispatcher: Arc::clone(&state.dispatcher),
        model_override: None,
    };

    // Streaming path (the TUI `/review`): the read-only loop runs for minutes, so
    // block-and-return would freeze the UI. Instead spawn the loop, return
    // immediately with the audit task id, and publish `AuditProgress` heartbeats
    // plus a terminal `AuditReport` on the dispatcher — the same spawn-and-stream
    // shape `quality.review` uses. `smj audit run` (the CLI) omits `stream`, so it
    // keeps the blocking path below and its long-timeout contract.
    let stream = params
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if stream {
        let report_path = params
            .get("report")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let task_id = session_id.to_string();
        let ingot = state.ingot.clone();
        let vault = Arc::clone(&state.vault);
        let embedder = Arc::clone(&state.embedder);
        let dispatcher = Arc::clone(&state.dispatcher);
        let spawn_task_id = task_id.clone();
        tokio::spawn(async move {
            let sink = AuditProgressSink {
                dispatcher: &dispatcher,
                turn_id: &spawn_task_id,
            };
            let outcome = run_audit_loop(
                &runner,
                &seed,
                &workspace,
                &session,
                &ingot,
                &vault,
                &embedder,
                &budget,
                Some(&sink),
            )
            .await;
            publish_audit_report(
                &dispatcher,
                &ingot,
                &spawn_task_id,
                &workspace,
                &report_path,
                outcome,
            )
            .await;
        });
        return Ok(json!({ "status": "review_started", "task_id": task_id }));
    }

    let findings = run_audit_loop(
        &runner,
        &seed,
        &workspace,
        &session,
        &state.ingot,
        &state.vault,
        &state.embedder,
        &budget,
        None,
    )
    .await?;

    persist_findings(&state.ingot, &session_id.to_string(), &findings).await?;

    respond(&params, &findings, &workspace).await
}

/// Persists findings, renders the report (writing it to `report_path` when set),
/// and publishes the terminal [`TurnEvent::AuditReport`] that ends the stream.
///
/// A loop error is surfaced as a `Failed` event carrying the reason, so the TUI
/// renders the failure instead of hanging on a stream that never terminates.
async fn publish_audit_report(
    dispatcher: &Dispatcher,
    ingot: &IngotHandle,
    task_id: &str,
    workspace: &Path,
    report_path: &Option<String>,
    outcome: Result<Vec<AuditFinding>, RpcError>,
) {
    use smedja_bellows::TurnEvent;

    let findings = match outcome {
        Ok(f) => f,
        Err(e) => {
            dispatcher.publish(TurnEvent::Failed {
                session_id: task_id.to_owned(),
                turn_id: task_id.to_owned(),
                reason: format!("audit failed: {e}"),
                correlation: CorrelationCtx::default(),
            });
            return;
        }
    };

    // Best-effort persistence; a failure here must not sink the report event.
    if let Err(e) = persist_findings(ingot, task_id, &findings).await {
        tracing::warn!(error = %e, "audit finding persistence failed");
    }

    let counts = severity_counts(&findings);
    let report = render_report(&findings);
    let written_path = match report_path {
        Some(rp) if !rp.is_empty() => match crate::executor::audit_report_path(workspace, rp) {
            Ok(full) => match tokio::fs::write(&full, &report).await {
                Ok(()) => Some(full.display().to_string()),
                Err(e) => {
                    tracing::warn!(error = %e, "audit report write failed");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "invalid audit report path");
                None
            }
        },
        _ => None,
    };

    dispatcher.publish(TurnEvent::AuditReport {
        report,
        counts,
        report_path: written_path,
        turn_id: Some(task_id.to_owned()),
        correlation: CorrelationCtx::default(),
    });
}

/// Builds the `audit.run` response, writing the report to `--report` when given.
async fn respond(
    params: &Value,
    findings: &[AuditFinding],
    workspace: &Path,
) -> Result<Value, RpcError> {
    let counts = severity_counts(findings);
    let format = params.get("format").and_then(Value::as_str).unwrap_or("md");

    if format == "json" {
        let typed = serde_json::to_value(findings)
            .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;
        return Ok(json!({ "findings": typed, "counts": counts }));
    }

    let report = render_report(findings);
    let typed = serde_json::to_value(findings)
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;

    if let Some(report_path) = params.get("report").and_then(Value::as_str) {
        if !report_path.is_empty() {
            let full = crate::executor::audit_report_path(workspace, report_path)
                .map_err(|e| RpcError::new(codes::INVALID_PARAMS, e))?;
            tokio::fs::write(&full, &report)
                .await
                .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;
            return Ok(json!({
                "findings": typed,
                "counts": counts,
                "report_path": full.display().to_string(),
            }));
        }
    }

    Ok(json!({ "findings": typed, "counts": counts, "report": report }))
}

/// Resolves the workspace root from the `workspace` param, falling back to
/// `SMEDJA_WORKSPACE` and then the current directory.
fn resolve_workspace(params: &Value) -> std::path::PathBuf {
    if let Some(ws) = params.get("workspace").and_then(Value::as_str) {
        if !ws.is_empty() {
            return std::path::PathBuf::from(ws);
        }
    }
    std::env::var("SMEDJA_WORKSPACE")
        .ok()
        .filter(|p| !p.is_empty())
        .map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            std::path::PathBuf::from,
        )
}

#[cfg(test)]
mod tests;
