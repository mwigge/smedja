//! Spec RPC handlers: `spec.create/validate/show/diff/list/archive/status`.
//!
//! These expose the native [`smedja_spec::SpecEngine`] over the JSON-RPC socket
//! so `smedja-tui` (and any other client) can author, validate, inspect, and
//! archive OpenSpec changes without shelling out to an external binary. Every
//! handler resolves the engine against the daemon's workspace root.

use serde_json::{json, Value};
use smedja_rpc::{codes, RpcError};
use smedja_spec::SpecEngine;

use crate::handlers::HandlerState;
use crate::missing_param;

/// Resolves the workspace-rooted spec engine for the daemon.
fn engine() -> SpecEngine {
    SpecEngine::at_workspace(&crate::common::workspace_root())
}

/// Maps a [`smedja_spec::SpecError`] to an RPC error.
fn spec_err(e: &smedja_spec::SpecError) -> RpcError {
    RpcError::new(codes::INVALID_PARAMS, e.to_string())
}

/// Reads the required `change` param.
fn change_param(params: &Value) -> Result<String, RpcError> {
    params["change"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| missing_param("change"))
}

/// Handles `spec.create`: scaffolds a new change directory.
///
/// # Errors
///
/// Returns an error when `change` is missing or the change already exists.
pub(crate) async fn create(_state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let change = change_param(&params)?;
    let why = params["why"].as_str().unwrap_or_default();
    let what = params["what"].as_str().unwrap_or_default();
    let files = engine()
        .create_change(&change, why, what)
        .map_err(|e| spec_err(&e))?;
    let files: Vec<String> = files.into_iter().map(|p| p.display().to_string()).collect();
    Ok(json!({ "change": change, "files": files }))
}

/// Handles `spec.validate`: structural validation, optionally `strict`.
///
/// # Errors
///
/// Returns an error when `change` is missing.
pub(crate) async fn validate(_state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let change = change_param(&params)?;
    let strict = params["strict"].as_bool().unwrap_or(false);
    let report = engine().validate(&change, strict);
    Ok(serde_json::to_value(&report).unwrap_or(Value::Null))
}

/// Handles `spec.show`: renders a change summary.
///
/// # Errors
///
/// Returns an error when `change` is missing.
pub(crate) async fn show(_state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let change = change_param(&params)?;
    Ok(json!({ "change": change, "text": engine().show(&change) }))
}

/// Handles `spec.diff`: renders a change's deltas as markdown.
///
/// # Errors
///
/// Returns an error when `change` is missing.
pub(crate) async fn diff(_state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let change = change_param(&params)?;
    Ok(json!({ "change": change, "text": engine().diff(&change) }))
}

/// Handles `spec.list`: enumerates active changes, archived changes, and specs.
///
/// # Errors
///
/// Never fails; the `Result` matches the handler signature.
pub(crate) async fn list(_state: HandlerState, _params: Value) -> Result<Value, RpcError> {
    let eng = engine();
    Ok(json!({
        "changes": eng.list_changes(),
        "archived": eng.list_archived(),
        "specs": eng.list_specs(),
    }))
}

/// Handles `spec.status`: per-change status, or all active changes when no
/// `change` is given.
///
/// # Errors
///
/// Never fails; the `Result` matches the handler signature.
pub(crate) async fn status(_state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let eng = engine();
    if let Some(change) = params["change"].as_str() {
        return Ok(serde_json::to_value(eng.status(change)).unwrap_or(Value::Null));
    }
    let statuses: Vec<Value> = eng
        .list_changes()
        .into_iter()
        .map(|c| serde_json::to_value(eng.status(&c)).unwrap_or(Value::Null))
        .collect();
    Ok(json!({ "changes": statuses }))
}

/// Handles `spec.archive`: merges deltas into the source specs and moves the
/// change into `changes/archive/`.
///
/// # Errors
///
/// Returns an error when `change` is missing or the archive operation fails.
pub(crate) async fn archive(_state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let change = change_param(&params)?;
    let outcome = engine().archive(&change).map_err(|e| spec_err(&e))?;
    Ok(json!({
        "change": outcome.change,
        "capabilities": outcome.capabilities,
        "archived_path": outcome.archived_path.display().to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use smedja_spec::SpecEngine;

    // The handlers resolve their engine from `workspace_root()`, so these tests
    // drive the engine directly (the handler bodies are one-line delegations);
    // an end-to-end tool round-trip is covered in the executor tests.
    #[test]
    fn create_then_validate_then_archive_round_trips_via_engine() {
        let tmp = tempfile::tempdir().unwrap();
        let eng = SpecEngine::at_workspace(tmp.path());
        eng.create_change("c", "why", "what").unwrap();
        eng.write_delta(
            "c",
            "widget",
            "## ADDED Requirements\n\n### Requirement: R\nThe system SHALL x.\n\n#### Scenario: S\n- THEN ok\n",
        )
        .unwrap();
        assert!(eng.validate("c", true).valid);
        let outcome = eng.archive("c").unwrap();
        assert_eq!(outcome.capabilities, vec!["widget".to_owned()]);
        assert!(eng.read_spec("widget").unwrap().has_requirement("R"));
    }
}
