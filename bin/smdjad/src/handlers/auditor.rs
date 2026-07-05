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

    let findings = run_audit_loop(
        &runner,
        &seed,
        &workspace,
        &session,
        &state.ingot,
        &state.vault,
        &state.embedder,
        &budget,
    )
    .await?;

    persist_findings(&state.ingot, &session_id.to_string(), &findings).await?;

    respond(&params, &findings, &workspace).await
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
