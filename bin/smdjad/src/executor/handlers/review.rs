//! The `review_run` agent tool — one quality bar, any language.
//!
//! Delegates the deterministic, polyglot half of the review to
//! [`smedja_review`]: detect the languages touched by the changed files, run
//! each language's canonical format / import-sort / lint tools on the changed
//! files only (skip-not-fail when a tool is absent), normalise every result
//! into the SARIF-shaped finding, and grade the change against the seven
//! uniform dimensions with thresholds from `.smedja/quality.toml`.
//!
//! This tool owns Stages 0/1/3 (detect + deterministic layer + the bar + SARIF)
//! so every runner grades against the identical bar. The LLM layer (Stage 2 —
//! SOLID / clean naming / correctness / maintainability) runs provider-side in
//! the auditor/quality RPC path, which merges its findings into this same bar
//! and finding list; those findings share the [`smedja_review::Finding`] shape.

use std::path::Path;
use std::process::Stdio;

use serde_json::{json, Value};
use smedja_review::{grade, languages_from_paths, run_deterministic, to_sarif, Thresholds};

/// Runs `review_run`: detect → deterministic layer → grade → merged report.
///
/// Input: `{ scope? }`.
/// - `scope` — `"changed"`/`"all"` selects the changed-file set: `"all"`
///   diffs against `HEAD~1` (the default), `"staged"` uses the index.
pub(crate) async fn review_run(input: &Value, workspace: &Path) -> String {
    let scope = input.get("scope").and_then(Value::as_str).unwrap_or("all");
    let changed = changed_paths(workspace, scope).await;

    if changed.is_empty() {
        return json!({
            "summary": "no changed files to review",
            "languages": Vec::<String>::new(),
            "findings": Vec::<Value>::new(),
        })
        .to_string();
    }

    let languages: Vec<&str> = languages_from_paths(&changed)
        .iter()
        .map(|l| l.label())
        .collect();

    let outcome = run_deterministic(workspace, &changed).await;

    // tdd/coverage dimension gate: does the diff add tests for its changes?
    let diff = crate::quality_hook::git_diff(workspace);
    let tests_cover_changes = !smedja_methodology::tdd::evaluate(&diff).is_advisory();

    let thresholds = Thresholds::load(workspace);
    // Coverage tooling is not wired into the deterministic layer yet; None means
    // the coverage floor is not enforced (the tests-exist gate still applies).
    let bar = grade(&outcome.findings, tests_cover_changes, None, &thresholds);
    let sarif = to_sarif(&outcome.findings);

    let findings_json: Vec<Value> = outcome
        .findings
        .iter()
        .map(finding_to_audit_shape)
        .collect();

    json!({
        "summary": format!(
            "grade {} ({}/{} dimensions) over {} changed file(s), {} language(s)",
            bar.grade,
            bar.passing(),
            bar.dimensions.len(),
            changed.len(),
            languages.len(),
        ),
        "pass": bar.pass,
        "grade": bar.grade.to_string(),
        "languages": languages,
        "bar": bar,
        "findings": findings_json,
        "tools_ran": outcome.ran,
        "tools_skipped": outcome.skipped,
        "sarif": sarif,
    })
    .to_string()
}

/// Renders a finding in the daemon's `AuditFinding` JSON shape so deterministic
/// findings merge with the LLM auditor's findings in one list.
fn finding_to_audit_shape(f: &smedja_review::Finding) -> Value {
    json!({
        "severity": f.severity.as_str(),
        "file": f.file,
        "line": f.line,
        "rule": f.rule,
        "rationale": f.rationale,
        "dimension": f.dimension.label(),
    })
}

/// Returns the workspace-relative changed paths for the requested scope.
async fn changed_paths(workspace: &Path, scope: &str) -> Vec<String> {
    let args: &[&str] = match scope {
        "staged" | "index" => &["diff", "--name-only", "--cached"],
        _ => &["diff", "--name-only", "HEAD~1"],
    };
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .output()
        .await;
    let Ok(out) = output else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_review::Language;

    #[tokio::test]
    async fn no_changes_reports_nothing_to_review() {
        // A non-git temp dir yields no changed paths → clean "nothing" response.
        let tmp = tempfile::tempdir().unwrap();
        let out = review_run(&json!({ "scope": "all" }), tmp.path()).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["summary"], "no changed files to review");
    }

    #[test]
    fn finding_shape_matches_audit_finding_fields() {
        let f = smedja_review::Finding::new(
            smedja_review::Severity::Medium,
            smedja_review::Dimension::Format,
            "src/a.rs",
            Some(4),
            "rustfmt",
            "not formatted",
        );
        let v = finding_to_audit_shape(&f);
        assert_eq!(v["severity"], "medium");
        assert_eq!(v["file"], "src/a.rs");
        assert_eq!(v["line"], 4);
        assert_eq!(v["rule"], "rustfmt");
        assert_eq!(v["dimension"], "format");
    }

    #[test]
    fn language_detection_over_changed_paths() {
        let paths = vec!["a.rs".to_owned(), "b.py".to_owned()];
        let langs = languages_from_paths(&paths);
        assert!(langs.contains(&Language::Rust));
        assert!(langs.contains(&Language::Python));
    }
}
