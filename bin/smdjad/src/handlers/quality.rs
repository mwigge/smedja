//! Quality RPC handlers: `quality.review`.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use smedja_bellows::event::CorrelationCtx;
use smedja_bellows::TurnEvent;
use smedja_rpc::{codes, RpcError};

use crate::handlers::HandlerState;
use crate::{quality_hook, quality_runner};

/// Handles `quality.review`: runs a Tier-2 LLM quality review for the
/// session's workspace and dispatches an updated `QualitySnapshot`.
///
/// # Errors
///
/// Returns an error when the session is not found in the ingot.
pub(crate) async fn review(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new(codes::INVALID_PARAMS, "session_id required"))?
        .to_owned();

    let session = state
        .ingot
        .get_session(&session_id)
        .await
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?
        .ok_or_else(|| RpcError::new(codes::INVALID_PARAMS, "session not found"))?;

    let workspace_root = session
        .workspace_root
        .as_deref()
        .map_or_else(|| PathBuf::from("."), PathBuf::from);

    let primary_provider = session.runner_override.unwrap_or_default();
    let reviewer_model = quality_runner::quality_reviewer_model(&primary_provider);

    let dispatcher = Arc::clone(&state.dispatcher);

    tokio::spawn(async move {
        let diff = quality_hook::git_diff(&workspace_root);
        let session_skills = quality_hook::discover_session_skills(&workspace_root);
        let threshold = quality_hook::load_file_size_threshold(&workspace_root);
        let changed_files = crate::quality_hook::changed_file_sizes_for_review(&workspace_root);

        let tier1 = smedja_methodology::quality_evaluate(
            &diff,
            &changed_files,
            &session_skills,
            Some(threshold),
        );

        let review = quality_runner::review_turn(&diff, tier1.score, reviewer_model).await;

        let event = TurnEvent::QualitySnapshot {
            score: review.score,
            tdd_pass: tier1.tdd_pass,
            clean_pass: tier1.clean_pass,
            file_advisories: vec![],
            skill_advisories: review.findings,
            llm_reviewed: review.llm_reviewed,
            turn_id: None,
            correlation: CorrelationCtx::default(),
        };
        dispatcher.publish(event);
    });

    Ok(json!({"status": "review_started", "reviewer_model": reviewer_model}))
}
