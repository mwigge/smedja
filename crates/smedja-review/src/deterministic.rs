//! Stage 1 — the polyglot deterministic layer.
//!
//! Per detected language, the canonical format / import-sort / lint tools are
//! run over the *changed files only*. Each tool sits behind a `which`
//! availability check: an absent tool is recorded as skipped, never a failure.
//! Every tool's output is normalised into the SARIF-shaped [`Finding`], and the
//! full set can be emitted as a SARIF log via [`to_sarif`].
//!
//! The parsers are pure functions over captured output, so they are unit-tested
//! against real tool output samples without spawning any process.

use std::path::Path;
use std::process::Stdio;

use serde_json::json;

use crate::detect::{languages_from_paths, paths_for_language, Language};
use crate::{bar::Dimension, Finding, Severity};

/// A parsed diagnostic tuple: `(file, optional 1-based line, rationale)`.
pub type Diagnostic = (String, Option<u32>, String);

/// The outcome of the deterministic layer: findings plus which tools ran.
#[derive(Debug, Clone, Default)]
pub struct DeterministicOutcome {
    /// Normalised findings from every tool that ran.
    pub findings: Vec<Finding>,
    /// Tool ids that ran (their binary was present).
    pub ran: Vec<String>,
    /// Tool ids skipped because their binary was absent.
    pub skipped: Vec<String>,
}

/// Runs the deterministic review tools for every language touched by
/// `changed_paths`, scoped to those changed files.
pub async fn run_deterministic(workspace: &Path, changed_paths: &[String]) -> DeterministicOutcome {
    let mut outcome = DeterministicOutcome::default();
    for lang in languages_from_paths(changed_paths) {
        let files = paths_for_language(changed_paths, lang);
        for spec in specs_for(lang) {
            run_one(workspace, &spec, &files, changed_paths, &mut outcome).await;
        }
    }
    outcome
}

/// Runs a single tool spec, appending its findings or recording a skip.
async fn run_one(
    workspace: &Path,
    spec: &ToolSpec,
    files: &[&str],
    changed_paths: &[String],
    outcome: &mut DeterministicOutcome,
) {
    if which::which(spec.program).is_err() {
        outcome.skipped.push(spec.id.to_owned());
        return;
    }
    let args = (spec.build)(files);
    let output = tokio::process::Command::new(spec.program)
        .args(&args)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .output()
        .await;
    outcome.ran.push(spec.id.to_owned());
    let Ok(out) = output else {
        return;
    };
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for (file, line, rationale) in (spec.parse)(&combined) {
        // Clean-as-you-code: keep only findings on changed files.
        if !is_changed(&file, changed_paths) {
            continue;
        }
        outcome.findings.push(Finding::new(
            spec.severity,
            spec.dimension,
            file,
            line,
            spec.id,
            rationale,
        ));
    }
}

/// `true` when `file` matches one of the changed paths (exact or suffix match,
/// to tolerate absolute vs workspace-relative reporting).
fn is_changed(file: &str, changed_paths: &[String]) -> bool {
    changed_paths
        .iter()
        .any(|c| c == file || file.ends_with(c.as_str()) || c.ends_with(file))
}

// ── Tool specifications ──────────────────────────────────────────────────────

/// A deterministic review tool: how to invoke it and how to parse its output.
struct ToolSpec {
    /// Stable id, used as the finding `rule` slug and in ran/skipped lists.
    id: &'static str,
    /// The binary to look up on `$PATH` and execute.
    program: &'static str,
    /// The dimension its findings score against.
    dimension: Dimension,
    /// Severity assigned to its findings.
    severity: Severity,
    /// Builds the argument vector from the changed file list.
    build: fn(&[&str]) -> Vec<String>,
    /// Parses combined output into `(file, line, rationale)` findings.
    parse: fn(&str) -> Vec<Diagnostic>,
}

/// The tool specs to run for `lang`.
fn specs_for(lang: Language) -> Vec<ToolSpec> {
    match lang {
        Language::Rust => vec![
            ToolSpec {
                id: "rustfmt",
                program: "rustfmt",
                dimension: Dimension::Format,
                severity: Severity::Low,
                build: |files| {
                    let mut a = vec![
                        "--check".to_owned(),
                        "--edition".to_owned(),
                        "2021".to_owned(),
                    ];
                    a.extend(files.iter().map(|s| (*s).to_owned()));
                    a
                },
                parse: parse_rustfmt,
            },
            ToolSpec {
                id: "clippy",
                program: "cargo-clippy",
                dimension: Dimension::Clean,
                severity: Severity::Medium,
                build: |_| {
                    vec![
                        "--message-format".to_owned(),
                        "short".to_owned(),
                        "--quiet".to_owned(),
                    ]
                },
                parse: parse_unix_lint,
            },
        ],
        Language::Python => vec![
            ToolSpec {
                id: "ruff-format",
                program: "ruff",
                dimension: Dimension::Format,
                severity: Severity::Low,
                build: |files| with_files(&["format", "--check"], files),
                parse: parse_ruff_format,
            },
            ToolSpec {
                id: "ruff-I",
                program: "ruff",
                dimension: Dimension::ImportSort,
                severity: Severity::Low,
                build: |files| with_files(&["check", "--select", "I"], files),
                parse: parse_ruff_check,
            },
            ToolSpec {
                id: "ruff",
                program: "ruff",
                dimension: Dimension::Clean,
                severity: Severity::Medium,
                build: |files| with_files(&["check"], files),
                parse: parse_ruff_check,
            },
        ],
        Language::JavaScript => vec![
            ToolSpec {
                id: "prettier",
                program: "prettier",
                dimension: Dimension::Format,
                severity: Severity::Low,
                build: |files| with_files(&["--check"], files),
                parse: parse_prettier,
            },
            ToolSpec {
                id: "eslint",
                program: "eslint",
                dimension: Dimension::Clean,
                severity: Severity::Medium,
                build: |files| with_files(&["-f", "unix"], files),
                parse: parse_unix_lint,
            },
        ],
        Language::Go => vec![
            ToolSpec {
                id: "gofmt",
                program: "gofmt",
                dimension: Dimension::Format,
                severity: Severity::Low,
                build: |files| with_files(&["-l"], files),
                parse: parse_path_list,
            },
            ToolSpec {
                id: "gci",
                program: "gci",
                dimension: Dimension::ImportSort,
                severity: Severity::Low,
                build: |files| with_files(&["list"], files),
                parse: parse_path_list,
            },
            ToolSpec {
                id: "golangci-lint",
                program: "golangci-lint",
                dimension: Dimension::Clean,
                severity: Severity::Medium,
                build: |_| {
                    vec![
                        "run".to_owned(),
                        "--out-format".to_owned(),
                        "line-number".to_owned(),
                    ]
                },
                parse: parse_unix_lint,
            },
        ],
    }
}

/// Prepends `prefix` args to the changed-file list.
fn with_files(prefix: &[&str], files: &[&str]) -> Vec<String> {
    let mut a: Vec<String> = prefix.iter().map(|s| (*s).to_owned()).collect();
    a.extend(files.iter().map(|s| (*s).to_owned()));
    a
}

// ── Parsers (pure) ───────────────────────────────────────────────────────────

/// Parses `rustfmt --check` output: `Diff in <file> at line <n>:`.
#[must_use]
pub fn parse_rustfmt(output: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Diff in ") {
            let file = rest.split(" at line ").next().unwrap_or(rest).to_owned();
            let n = rest
                .split(" at line ")
                .nth(1)
                .and_then(|s| s.trim_end_matches(':').trim().parse().ok());
            out.push((file, n, "not formatted (rustfmt --check)".to_owned()));
        }
    }
    out
}

/// Parses `ruff format --check`: `Would reformat: <file>`.
#[must_use]
pub fn parse_ruff_format(output: &str) -> Vec<Diagnostic> {
    output
        .lines()
        .filter_map(|l| {
            l.trim().strip_prefix("Would reformat:").map(|f| {
                (
                    f.trim().to_owned(),
                    None,
                    "not formatted (ruff format)".to_owned(),
                )
            })
        })
        .collect()
}

/// Parses `ruff check` text: `<file>:<line>:<col>: <CODE> <message>`.
#[must_use]
pub fn parse_ruff_check(output: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in output.lines() {
        let Some((file, rest)) = split_location(line.trim()) else {
            continue;
        };
        let (line_no, tail) = rest;
        out.push((file, line_no, tail));
    }
    out
}

/// Parses `prettier --check`: `[warn] <file>`.
#[must_use]
pub fn parse_prettier(output: &str) -> Vec<Diagnostic> {
    output
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            t.strip_prefix("[warn]")
                .map(str::trim)
                .filter(|f| !f.contains(' ') && f.contains('.'))
                .map(|f| (f.to_owned(), None, "not formatted (prettier)".to_owned()))
        })
        .collect()
}

/// Parses unix-format lint output (`eslint -f unix`, `golangci-lint`,
/// `cargo clippy --message-format short`): `<file>:<line>:<col>: <message>`.
#[must_use]
pub fn parse_unix_lint(output: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in output.lines() {
        let t = line.trim();
        // Skip clippy/eslint summary/help lines.
        if t.is_empty() || t.starts_with("warning: ") && !t.contains(':') {
            continue;
        }
        if let Some((file, (line_no, msg))) = split_location(t) {
            if !msg.is_empty() {
                out.push((file, line_no, msg));
            }
        }
    }
    out
}

/// Parses a plain newline-separated list of file paths (`gofmt -l`, `gci list`).
#[must_use]
pub fn parse_path_list(output: &str) -> Vec<Diagnostic> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.contains('.') && !l.contains(' '))
        .map(|f| {
            (
                f.to_owned(),
                None,
                "needs formatting/import-sort".to_owned(),
            )
        })
        .collect()
}

/// Splits a `file:line:col: message` diagnostic into `(file, (line, message))`.
///
/// Tolerates Windows drive letters by requiring the line field to be numeric.
fn split_location(line: &str) -> Option<(String, (Option<u32>, String))> {
    let mut parts = line.splitn(4, ':');
    let file = parts.next()?.trim();
    let line_no: u32 = parts.next()?.trim().parse().ok()?;
    let _col = parts.next()?; // column, ignored
    let message = parts.next().unwrap_or("").trim().to_owned();
    if file.is_empty() {
        return None;
    }
    Some((file.to_owned(), (Some(line_no), message)))
}

// ── SARIF emission ───────────────────────────────────────────────────────────

/// Emits findings as a minimal SARIF 2.1.0 log document.
#[must_use]
pub fn to_sarif(findings: &[Finding]) -> serde_json::Value {
    let results: Vec<serde_json::Value> = findings
        .iter()
        .map(|f| {
            let mut region = json!({});
            if let Some(line) = f.line {
                region = json!({ "startLine": line });
            }
            json!({
                "ruleId": f.rule,
                "level": f.severity.sarif_level(),
                "message": { "text": f.rationale },
                "properties": { "dimension": f.dimension.label() },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": f.file },
                        "region": region
                    }
                }]
            })
        })
        .collect();
    json!({
        "version": "2.1.0",
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "runs": [{
            "tool": { "driver": { "name": "smedja-review", "rules": [] } },
            "results": results
        }]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustfmt_check_parses_diffs() {
        let out = "Diff in /w/src/main.rs at line 12:\n Diff in src/lib.rs at line 3:";
        let f = parse_rustfmt(out);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].0, "/w/src/main.rs");
        assert_eq!(f[0].1, Some(12));
    }

    #[test]
    fn ruff_format_parses_would_reformat() {
        let out = "Would reformat: app/api.py\n1 file would be reformatted";
        let f = parse_ruff_format(out);
        assert_eq!(
            f,
            vec![(
                "app/api.py".to_owned(),
                None,
                "not formatted (ruff format)".to_owned()
            )]
        );
    }

    #[test]
    fn ruff_check_parses_code_and_message() {
        let out = "app/api.py:3:1: I001 Import block is un-sorted\napp/api.py:9:5: F401 imported but unused";
        let f = parse_ruff_check(out);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].0, "app/api.py");
        assert_eq!(f[0].1, Some(3));
        assert!(f[0].2.contains("I001"));
    }

    #[test]
    fn prettier_parses_warn_lines() {
        let out = "Checking formatting...\n[warn] web/ui.tsx\n[warn] Code style issues found";
        let f = parse_prettier(out);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].0, "web/ui.tsx");
    }

    #[test]
    fn unix_lint_parses_eslint_and_clippy_shapes() {
        let out = "src/a.js:10:5: 'x' is defined but never used [no-unused-vars]\nsrc/lib.rs:4:1: warning: unused variable: `y`";
        let f = parse_unix_lint(out);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].0, "src/a.js");
        assert_eq!(f[0].1, Some(10));
        assert_eq!(f[1].0, "src/lib.rs");
    }

    #[test]
    fn path_list_parses_gofmt_output() {
        let out = "cmd/serve.go\ninternal/util.go\n";
        let f = parse_path_list(out);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].0, "cmd/serve.go");
    }

    #[test]
    fn is_changed_matches_relative_and_absolute() {
        let changed = vec!["src/main.rs".to_owned()];
        assert!(is_changed("src/main.rs", &changed));
        assert!(is_changed("/repo/src/main.rs", &changed));
        assert!(!is_changed("src/other.rs", &changed));
    }

    #[test]
    fn sarif_emits_results_with_dimension_property() {
        let findings = vec![Finding::new(
            Severity::Low,
            Dimension::Format,
            "a.rs",
            Some(3),
            "rustfmt",
            "not formatted",
        )];
        let sarif = to_sarif(&findings);
        assert_eq!(sarif["version"], "2.1.0");
        let result = &sarif["runs"][0]["results"][0];
        assert_eq!(result["ruleId"], "rustfmt");
        assert_eq!(result["level"], "warning");
        assert_eq!(result["properties"]["dimension"], "format");
        assert_eq!(
            result["locations"][0]["physicalLocation"]["region"]["startLine"],
            3
        );
    }
}
