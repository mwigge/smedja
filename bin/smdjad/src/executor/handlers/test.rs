//! The `test_run` agent tool — universal test discovery and execution.
//!
//! Delegates to [`smedja_testkit`]: detect every test suite under the workspace
//! (across languages, delegating to a monorepo meta-runner when present), run
//! each requesting a machine-readable format, and normalise the results into a
//! single [`smedja_testkit::TestReport`]. Every runner on the shared tool seam
//! gets the identical discovery + parsing, so the TUI `/test` command and the
//! loop's fix guide can consume one structure.

use std::path::Path;

use serde_json::{json, Value};
use smedja_testkit::{detect_suites, run_suite, Scope, SuiteReport, TestReport};

/// Runs `test_run`: detect suites, run them, and return a normalised report.
///
/// Input: `{ suite?, scope?, changed_since? }`.
/// - `suite` — optional runner label filter (`cargo`, `npm`, `go`, `pytest`, …).
/// - `scope` — `"all"` (default) or `"affected"`.
/// - `changed_since` — git ref for affected mode's native selectors.
pub(crate) async fn test_run(input: &Value, workspace: &Path) -> String {
    let scope = input
        .get("scope")
        .and_then(Value::as_str)
        .map_or(Scope::All, Scope::parse);
    let changed_since = input
        .get("changed_since")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    let suite_filter = input
        .get("suite")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());

    let mut suites = detect_suites(workspace);
    if let Some(filter) = suite_filter {
        suites.retain(|s| s.runner.label().eq_ignore_ascii_case(filter));
    }

    if suites.is_empty() {
        let msg = suite_filter.map_or_else(
            || "no test suites detected in workspace".to_owned(),
            |f| format!("no test suite matching '{f}' detected in workspace"),
        );
        return json!({ "report": TestReport::default(), "summary": msg }).to_string();
    }

    let mut report = TestReport::default();
    for suite in &suites {
        let result: SuiteReport = run_suite(workspace, suite, scope, changed_since).await;
        report.suites.push(result);
    }

    json!({
        "report": report,
        "summary": report.summary(),
        "green": report.green(),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_workspace_reports_no_suites() {
        let tmp = tempfile::tempdir().unwrap();
        let out = test_run(&json!({}), tmp.path()).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["summary"].as_str().unwrap().contains("no test suites"));
    }

    #[tokio::test]
    async fn suite_filter_narrows_detection() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("go.mod"), "module x\n").unwrap();
        // Filter to a runner that is not present → explicit "no suite matching".
        let out = test_run(&json!({ "suite": "cargo" }), tmp.path()).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["summary"].as_str().unwrap().contains("matching 'cargo'"));
    }

    #[tokio::test]
    async fn detects_go_suite_and_serialises_report() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("go.mod"), "module x\n").unwrap();
        let out = test_run(&json!({ "suite": "go" }), tmp.path()).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        // Whether or not `go` is installed, a suite entry is produced.
        assert_eq!(v["report"]["suites"][0]["runner"], "go");
    }
}
