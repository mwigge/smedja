//! Suite execution: run each detected suite requesting a machine-readable
//! format, then normalise via [`crate::parse`] into a [`crate::SuiteReport`].
//!
//! Every runner is behind a `which` availability check — a missing toolchain
//! yields a `skipped` suite with a note rather than a hard failure, so a
//! polyglot repo still reports on the toolchains that are present.

use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use crate::detect::{detect_suites, Runner, Suite};
use crate::parse::{
    parse_cargo_text, parse_go_json, parse_jest_json, parse_junit_xml, parse_lenient_text, Parsed,
};
use crate::{SuiteReport, TestReport};

/// Selects how much of a suite to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// Run the entire suite.
    #[default]
    All,
    /// Run only tests affected by recent changes, per the runner's native
    /// selector, falling back to a full run when no safe selector exists.
    Affected,
}

impl Scope {
    /// Parses the `scope` tool argument (`"all"` / `"affected"`).
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "affected" | "changed" => Self::Affected,
            _ => Self::All,
        }
    }
}

/// Detects and runs every suite under `root`, returning a merged report.
pub async fn run_all(root: &Path, scope: Scope, changed_since: Option<&str>) -> TestReport {
    let mut suites = Vec::new();
    for suite in detect_suites(root) {
        suites.push(run_suite(root, &suite, scope, changed_since).await);
    }
    TestReport { suites }
}

/// Runs one suite and normalises the result.
pub async fn run_suite(
    root: &Path,
    suite: &Suite,
    scope: Scope,
    changed_since: Option<&str>,
) -> SuiteReport {
    let plan = command_for(suite.runner, scope, changed_since);
    let dir = rel_dir(root, &suite.dir);

    if which::which(plan.program).is_err() {
        return SuiteReport {
            runner: suite.runner.label().to_owned(),
            dir,
            note: Some(format!("{} not installed; skipped", plan.program)),
            ..SuiteReport::default()
        };
    }

    let started = Instant::now();
    let output = tokio::process::Command::new(plan.program)
        .args(&plan.args)
        .current_dir(&suite.dir)
        .stdin(Stdio::null())
        .output()
        .await;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    let mut report = SuiteReport {
        runner: suite.runner.label().to_owned(),
        dir,
        duration_ms,
        ..SuiteReport::default()
    };

    match output {
        Err(e) => {
            report.note = Some(format!("failed to spawn {}: {e}", plan.program));
        }
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let combined = format!("{stdout}\n{stderr}");
            let parsed = (plan.parser)(&combined);
            apply_parsed(&mut report, parsed);
            // A non-zero exit with no parsed failures means the run itself broke
            // (compile error, config error) — surface it rather than reporting
            // a false "0 failed".
            if !out.status.success() && report.failed == 0 && report.passed == 0 {
                report.failed = report.failed.max(1);
                report.failures.push(crate::Failure {
                    name: format!("{} run", suite.runner.label()),
                    message: last_lines(&combined, 3),
                });
                report.note = Some("runner exited non-zero with no parsed tests".to_owned());
            }
        }
    }
    report
}

/// Copies a [`Parsed`] tally into a [`SuiteReport`].
fn apply_parsed(report: &mut SuiteReport, parsed: Parsed) {
    report.passed = parsed.passed;
    report.failed = parsed.failed;
    report.skipped = parsed.skipped;
    report.failures = parsed.failures;
    if let Some(ms) = parsed.duration_ms {
        report.duration_ms = ms;
    }
}

/// A concrete command plan for a runner: program, args, and the parser to apply.
struct Plan {
    program: &'static str,
    args: Vec<String>,
    parser: fn(&str) -> Parsed,
}

/// Maps a runner + scope to the command to execute and the parser for its
/// output. Prefers a per-framework machine format; falls back to lenient text
/// or JUnit-XML parsing where injecting a format is not possible.
fn command_for(runner: Runner, scope: Scope, changed_since: Option<&str>) -> Plan {
    let affected = matches!(scope, Scope::Affected);
    let sel = affected_selector(runner, changed_since);
    let use_affected = affected && sel.is_some();
    let extra = if use_affected {
        sel.unwrap()
    } else {
        Vec::new()
    };

    match runner {
        Runner::Cargo => Plan {
            program: "cargo",
            args: vec!["test".into(), "--no-fail-fast".into()],
            parser: parse_cargo_text,
        },
        Runner::Go => {
            let mut args = vec!["test".into(), "-json".into()];
            if use_affected {
                args.extend(extra);
            } else {
                args.push("./...".into());
            }
            Plan {
                program: "go",
                args,
                parser: parse_go_json,
            }
        }
        Runner::Pytest => {
            let mut args = vec!["-q".into()];
            args.extend(extra);
            Plan {
                program: "pytest",
                args,
                parser: parse_lenient_text,
            }
        }
        Runner::Npm => {
            let mut args = vec!["test".into(), "--silent".into()];
            if use_affected {
                args.push("--".into());
                args.extend(extra);
            }
            Plan {
                program: "npm",
                args,
                parser: npm_parser,
            }
        }
        Runner::Maven => Plan {
            program: "mvn",
            args: vec!["-q".into(), "test".into()],
            parser: parse_junit_xml,
        },
        Runner::Gradle => Plan {
            program: "gradle",
            args: vec!["test".into(), "--console=plain".into()],
            parser: parse_lenient_text,
        },
        Runner::DotNet => Plan {
            program: "dotnet",
            args: vec!["test".into()],
            parser: parse_lenient_text,
        },
        Runner::Nx => Plan {
            program: "nx",
            args: if affected {
                vec!["affected".into(), "-t".into(), "test".into()]
            } else {
                vec!["run-many".into(), "-t".into(), "test".into()]
            },
            parser: parse_lenient_text,
        },
        Runner::Turbo => Plan {
            program: "turbo",
            args: vec!["run".into(), "test".into()],
            parser: parse_lenient_text,
        },
        Runner::Moon => Plan {
            program: "moon",
            args: vec![":test".into()],
            parser: parse_lenient_text,
        },
        Runner::Just => Plan {
            program: "just",
            args: vec!["test".into()],
            parser: parse_lenient_text,
        },
        Runner::Task => Plan {
            program: "task",
            args: vec!["test".into()],
            parser: parse_lenient_text,
        },
    }
}

/// npm's `test` script output is framework-defined: try jest's JSON shape first,
/// then fall back to lenient console parsing.
fn npm_parser(output: &str) -> Parsed {
    let jest = parse_jest_json(output);
    if jest.passed + jest.failed + jest.skipped > 0 {
        return jest;
    }
    parse_lenient_text(output)
}

/// Returns the runner's native affected-tests selector arguments, or `None`
/// when there is no safe selector — in which case the caller runs the full
/// suite. A missing or lockfile/config-only `changed_since` also forces full.
#[must_use]
pub fn affected_selector(runner: Runner, changed_since: Option<&str>) -> Option<Vec<String>> {
    let base = changed_since.filter(|s| !s.trim().is_empty())?;
    match runner {
        Runner::Nx => Some(vec![format!("--base={base}")]),
        Runner::Npm => Some(vec!["--changedSince".into(), base.to_owned()]),
        Runner::Pytest => Some(vec!["--testmon".into()]),
        // No reliable native change-selector: run everything.
        Runner::Cargo
        | Runner::Go
        | Runner::Maven
        | Runner::Gradle
        | Runner::DotNet
        | Runner::Turbo
        | Runner::Moon
        | Runner::Just
        | Runner::Task => None,
    }
}

/// `true` when a changed path forces a full run regardless of affected mode:
/// lockfiles, build manifests, and CI/tool config invalidate incremental
/// selection, so a safe implementation falls back to running everything.
#[must_use]
pub fn forces_full_run(changed_paths: &[String]) -> bool {
    const TRIGGERS: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "go.sum",
        "poetry.lock",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "tsconfig.json",
        "jest.config",
        "pytest.ini",
        "tox.ini",
        "nx.json",
        "turbo.json",
    ];
    changed_paths.iter().any(|p| {
        let name = Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(p);
        TRIGGERS.iter().any(|t| name == *t || name.starts_with(t))
    })
}

/// The workspace-relative display path for a suite directory.
fn rel_dir(root: &Path, dir: &Path) -> String {
    let rel = dir.strip_prefix(root).unwrap_or(dir);
    if rel.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        rel.to_string_lossy().into_owned()
    }
}

/// The last `n` non-empty lines of `text`, joined — a compact error tail.
fn last_lines(text: &str, n: usize) -> String {
    let mut lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let start = lines.len().saturating_sub(n);
    lines.drain(..start);
    lines.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_parses_affected_aliases() {
        assert_eq!(Scope::parse("affected"), Scope::Affected);
        assert_eq!(Scope::parse("changed"), Scope::Affected);
        assert_eq!(Scope::parse("all"), Scope::All);
        assert_eq!(Scope::parse(""), Scope::All);
    }

    #[test]
    fn affected_selector_per_runner() {
        assert_eq!(
            affected_selector(Runner::Npm, Some("main")),
            Some(vec!["--changedSince".into(), "main".into()])
        );
        assert_eq!(
            affected_selector(Runner::Pytest, Some("main")),
            Some(vec!["--testmon".into()])
        );
        assert_eq!(
            affected_selector(Runner::Nx, Some("origin/main")),
            Some(vec!["--base=origin/main".into()])
        );
        // No base → full run.
        assert_eq!(affected_selector(Runner::Npm, None), None);
        // No native selector → full run.
        assert_eq!(affected_selector(Runner::Cargo, Some("main")), None);
    }

    #[test]
    fn lockfile_changes_force_full_run() {
        assert!(forces_full_run(&["Cargo.lock".into()]));
        assert!(forces_full_run(&["frontend/package-lock.json".into()]));
        assert!(forces_full_run(&["go.mod".into()]));
        assert!(!forces_full_run(&["src/main.rs".into()]));
    }

    #[tokio::test]
    async fn missing_toolchain_is_skipped_not_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let suite = Suite {
            runner: Runner::Maven,
            dir: tmp.path().to_path_buf(),
        };
        // `mvn` is not installed in the test environment.
        if which::which("mvn").is_ok() {
            return;
        }
        let report = run_suite(tmp.path(), &suite, Scope::All, None).await;
        assert_eq!(report.failed, 0);
        assert_eq!(report.passed, 0);
        assert!(report.note.as_deref().unwrap().contains("not installed"));
    }

    #[test]
    fn affected_cargo_falls_back_to_full_command() {
        // Cargo has no native selector, so affected scope still runs full.
        let plan = command_for(Runner::Cargo, Scope::Affected, Some("main"));
        assert!(plan.args.contains(&"--no-fail-fast".to_owned()));
    }
}
