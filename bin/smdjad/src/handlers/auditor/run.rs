//! The `audit.run` RPC entry point: resolve scope → seed → loop → dedup →
//! persist → render.

use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};
use smedja_ingot::Session;
use smedja_rpc::{codes, RpcError};
use smedja_types::Timestamp;
use uuid::Uuid;

use super::findings::{render_report, severity_counts, AuditFinding};
use super::persist::persist_findings;
use super::provider::ProviderReviewTurn;
use super::review_loop::{run_audit_loop, LoopBudget};
use super::scope::{build_seed, resolve_scope};
use super::{DEFAULT_MAX_ITERATIONS, DEFAULT_TOKEN_BUDGET};
use crate::handlers::HandlerState;

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
mod tests {
    use super::super::findings::Severity;
    use super::*;

    fn sample_findings() -> Vec<AuditFinding> {
        vec![
            AuditFinding {
                severity: Severity::Critical,
                file: "src/a.rs".to_owned(),
                line: Some(10),
                rule: "sql-injection".to_owned(),
                rationale: "interpolated SQL".to_owned(),
            },
            AuditFinding {
                severity: Severity::Low,
                file: "src/b.rs".to_owned(),
                line: None,
                rule: "naming".to_owned(),
                rationale: "abbreviated name".to_owned(),
            },
        ]
    }

    #[tokio::test]
    async fn respond_inline_returns_report_and_counts() {
        let ws = tempfile::tempdir().unwrap();
        let resp = respond(&json!({}), &sample_findings(), ws.path())
            .await
            .unwrap();
        assert!(
            resp.get("report").is_some(),
            "inline report must be present"
        );
        assert!(resp["report"].as_str().unwrap().contains("## Summary"));
        assert_eq!(resp["counts"]["critical"], 1);
        assert!(resp["findings"].is_array());
    }

    #[tokio::test]
    async fn respond_format_json_emits_typed_findings_without_loss() {
        let ws = tempfile::tempdir().unwrap();
        let resp = respond(&json!({ "format": "json" }), &sample_findings(), ws.path())
            .await
            .unwrap();
        assert!(
            resp.get("report").is_none(),
            "json format has no markdown body"
        );
        let arr = resp["findings"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["severity"], "critical");
        assert_eq!(arr[0]["file"], "src/a.rs");
        assert_eq!(arr[0]["line"], 10);
        assert_eq!(arr[0]["rule"], "sql-injection");
        assert_eq!(arr[0]["rationale"], "interpolated SQL");
    }

    #[tokio::test]
    async fn respond_writes_report_to_path() {
        let ws = tempfile::tempdir().unwrap();
        // Canonicalise so the workspace-boundary check (which canonicalises the
        // root) accepts the not-yet-existing report path on platforms where the
        // temp dir is itself a symlink (e.g. /var → /private/var on macOS).
        let ws_root = ws.path().canonicalize().unwrap();
        let resp = respond(
            &json!({ "report": "report.md" }),
            &sample_findings(),
            &ws_root,
        )
        .await
        .unwrap();
        let path = resp["report_path"].as_str().unwrap();
        let written = std::fs::read_to_string(path).unwrap();
        assert!(
            written.contains("## Summary"),
            "report file must hold markdown"
        );
        assert!(
            resp.get("report").is_none(),
            "path mode does not inline the body"
        );
        assert_eq!(resp["counts"]["low"], 1);
    }
}
