//! The `spec_*` agent tools — native OpenSpec authoring from any runner.
//!
//! These delegate to [`smedja_spec::SpecEngine`] over the tool-execution
//! workspace, so an agent on any runner can author, validate, inspect, and
//! archive OpenSpec changes through the same engine the `spec.*` RPC and the TUI
//! `/spec` command use. Deltas themselves are authored with the ordinary
//! `write_file` tool at `openspec/changes/<name>/specs/<capability>/spec.md`;
//! these tools scaffold, validate, render, and merge them.

use std::path::Path;

use serde_json::{json, Value};
use smedja_spec::SpecEngine;

/// Resolves the engine over the tool-execution workspace.
fn engine(workspace: &Path) -> SpecEngine {
    SpecEngine::at_workspace(workspace)
}

/// Reads the required `change` argument, or returns a client-facing error JSON.
fn require_change(input: &Value) -> Result<String, String> {
    input
        .get("change")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| json!({ "error": "missing required argument: change" }).to_string())
}

/// `spec_create` — scaffold a new change's `proposal.md`/`design.md`/`tasks.md`.
pub(crate) fn spec_create(input: &Value, workspace: &Path) -> String {
    let change = match require_change(input) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let why = input.get("why").and_then(Value::as_str).unwrap_or_default();
    let what = input
        .get("what")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match engine(workspace).create_change(&change, why, what) {
        Ok(files) => json!({
            "change": change,
            "created": true,
            "files": files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        })
        .to_string(),
        Err(e) => json!({ "error": e.to_string() }).to_string(),
    }
}

/// `spec_validate` — structural validation, optionally `strict`.
pub(crate) fn spec_validate(input: &Value, workspace: &Path) -> String {
    let change = match require_change(input) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let strict = input
        .get("strict")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let report = engine(workspace).validate(&change, strict);
    serde_json::to_string(&report).unwrap_or_else(|e| json!({ "error": e.to_string() }).to_string())
}

/// `spec_show` — a human-readable change summary.
pub(crate) fn spec_show(input: &Value, workspace: &Path) -> String {
    let change = match require_change(input) {
        Ok(c) => c,
        Err(e) => return e,
    };
    json!({ "change": change, "text": engine(workspace).show(&change) }).to_string()
}

/// `spec_diff` — render a change's deltas as markdown.
pub(crate) fn spec_diff(input: &Value, workspace: &Path) -> String {
    let change = match require_change(input) {
        Ok(c) => c,
        Err(e) => return e,
    };
    json!({ "change": change, "text": engine(workspace).diff(&change) }).to_string()
}

/// `spec_list` — active changes, archived changes, and known specs.
pub(crate) fn spec_list(_input: &Value, workspace: &Path) -> String {
    let eng = engine(workspace);
    json!({
        "changes": eng.list_changes(),
        "archived": eng.list_archived(),
        "specs": eng.list_specs(),
    })
    .to_string()
}

/// `spec_status` — per-change status, or all active changes when no `change`.
pub(crate) fn spec_status(input: &Value, workspace: &Path) -> String {
    let eng = engine(workspace);
    if let Some(change) = input.get("change").and_then(Value::as_str) {
        return serde_json::to_string(&eng.status(change))
            .unwrap_or_else(|e| json!({ "error": e.to_string() }).to_string());
    }
    let statuses: Vec<_> = eng
        .list_changes()
        .into_iter()
        .map(|c| eng.status(&c))
        .collect();
    json!({ "changes": statuses }).to_string()
}

/// `spec_archive` — merge a change's deltas into the source specs and move it to
/// `changes/archive/`.
pub(crate) fn spec_archive(input: &Value, workspace: &Path) -> String {
    let change = match require_change(input) {
        Ok(c) => c,
        Err(e) => return e,
    };
    match engine(workspace).archive(&change) {
        Ok(outcome) => json!({
            "change": outcome.change,
            "archived": true,
            "capabilities": outcome.capabilities,
            "archived_path": outcome.archived_path.display().to_string(),
        })
        .to_string(),
        Err(e) => json!({ "error": e.to_string() }).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_validate_archive_tools_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();

        // Scaffold via the tool.
        let created: Value =
            serde_json::from_str(&spec_create(&json!({ "change": "c", "why": "w" }), ws)).unwrap();
        assert_eq!(created["created"], json!(true));

        // Author a delta with the ordinary file surface.
        let delta_path = ws.join("openspec/changes/c/specs/widget/spec.md");
        std::fs::create_dir_all(delta_path.parent().unwrap()).unwrap();
        std::fs::write(
            &delta_path,
            "## ADDED Requirements\n\n### Requirement: R\nThe system SHALL x.\n\n#### Scenario: S\n- THEN ok\n",
        )
        .unwrap();

        // Validate --strict passes.
        let report: Value = serde_json::from_str(&spec_validate(
            &json!({ "change": "c", "strict": true }),
            ws,
        ))
        .unwrap();
        assert_eq!(report["valid"], json!(true), "report was {report}");

        // spec_list sees the change.
        let listed: Value = serde_json::from_str(&spec_list(&json!({}), ws)).unwrap();
        assert!(listed["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "c"));

        // Archive merges the delta and moves the change.
        let archived: Value =
            serde_json::from_str(&spec_archive(&json!({ "change": "c" }), ws)).unwrap();
        assert_eq!(archived["archived"], json!(true));
        assert!(ws.join("openspec/specs/widget/spec.md").is_file());
        assert!(ws.join("openspec/changes/archive/c").is_dir());
    }

    #[test]
    fn missing_change_argument_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let out: Value = serde_json::from_str(&spec_show(&json!({}), tmp.path())).unwrap();
        assert!(out["error"].as_str().unwrap().contains("change"));
    }
}
