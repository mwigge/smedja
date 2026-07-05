//! Failure mining — persists loop failure patterns as Markdown guides.
//!
//! After a `LoopFailed` event, the engine calls [`write_failure_guide`] to
//! store role-scoped failure patterns under `.smedja/guides/<role>.md`.
//! These files are read by agents on subsequent attempts to avoid repeating
//! the same mistakes.

use std::path::Path;

use anyhow::Result;

use crate::verify::VerifyResult;

/// Maximum number of trailing lines of verification output embedded in a guide.
const MAX_OUTPUT_LINES: usize = 100;
/// Maximum number of trailing bytes of verification output embedded in a guide.
const MAX_OUTPUT_BYTES: usize = 8 * 1024;
/// Maximum number of failing test names surfaced at the top of a guide.
const MAX_FAILING_TESTS: usize = 20;

/// Writes failure patterns for `role` to `.smedja/guides/<role>.md` inside
/// `workspace`.
///
/// When `verification` is `Some`, the actual output captured from the failing
/// verification command is embedded in a `## Verification output` section so the
/// fix role sees *what* failed rather than only *that* it failed. The output is
/// tail-truncated (see [`MAX_OUTPUT_LINES`] / [`MAX_OUTPUT_BYTES`]) to keep the
/// guide bounded, and any recognisable failing test names are surfaced near the
/// top. A timeout is called out explicitly; empty output falls back to the
/// generic patterns-only guide. Pass `None` for failures that have no command
/// output to attach (e.g. a reviewer rejection).
///
/// Creates the guides directory if it does not yet exist.  An empty `patterns`
/// slice writes a guide with an empty patterns section rather than failing.
///
/// # Errors
///
/// Returns an error when the directory cannot be created or the file cannot be
/// written.
pub fn write_failure_guide(
    role: &str,
    patterns: &[String],
    verification: Option<&VerifyResult>,
    workspace: &Path,
) -> Result<()> {
    let guides_dir = workspace.join(".smedja").join("guides");
    std::fs::create_dir_all(&guides_dir)?;

    let path = guides_dir.join(format!("{role}.md"));
    let pattern_lines = if patterns.is_empty() {
        String::new()
    } else {
        patterns
            .iter()
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut content = format!(
        "# Failure guide for `{role}`\n\nLast updated by loop engine after failure.\n\n## Patterns\n\n{pattern_lines}\n"
    );

    if let Some(result) = verification {
        let section = render_verification(result);
        if !section.is_empty() {
            content.push('\n');
            content.push_str(&section);
        }
    }

    std::fs::write(path, content)?;
    Ok(())
}

/// Renders the `## Verification output` section for a failing [`VerifyResult`].
///
/// Returns an empty string when there is nothing useful to embed (no timeout and
/// both streams empty), so the caller keeps the generic patterns-only guide.
fn render_verification(result: &VerifyResult) -> String {
    if result.timed_out {
        return "## Verification output\n\nThe verification command TIMED OUT before completing, \
             so no pass/fail signal was produced. Investigate slow, hanging, or deadlocked \
             tests rather than assuming a logic error.\n"
            .to_owned();
    }

    let stdout = result.stdout.trim_end();
    let stderr = result.stderr.trim_end();
    if stdout.is_empty() && stderr.is_empty() {
        return String::new();
    }

    let mut section = String::from("## Verification output\n\n");
    section.push_str(&format!(
        "The verification command exited with code {}. Address the failures below; \
         do not guess.\n",
        result.exit_code
    ));

    // Reuse the shared testkit parser to normalise the verification output into
    // a structured pass/fail tally when it recognises a known test format.
    if let Some(summary) = normalized_test_summary(stdout, stderr) {
        section.push_str(&format!("\nNormalised test report: {summary}\n"));
    }

    let failing = failing_test_names(stdout, stderr);
    if !failing.is_empty() {
        section.push_str("\n### Failing tests\n\n");
        for name in &failing {
            section.push_str(&format!("- `{name}`\n"));
        }
    }

    if !stdout.is_empty() {
        section.push_str("\n### stdout\n\n```\n");
        section.push_str(&tail_truncate(stdout));
        section.push_str("\n```\n");
    }
    if !stderr.is_empty() {
        section.push_str("\n### stderr\n\n```\n");
        section.push_str(&tail_truncate(stderr));
        section.push_str("\n```\n");
    }

    section
}

/// Tail-truncates `text` to the last [`MAX_OUTPUT_LINES`] lines and, after that,
/// the last [`MAX_OUTPUT_BYTES`] bytes (on a char boundary). The tail is kept
/// because verification tools print the failing summary last. A marker is
/// prepended when anything was dropped.
fn tail_truncate(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let (mut kept, mut truncated) = if lines.len() > MAX_OUTPUT_LINES {
        (lines[lines.len() - MAX_OUTPUT_LINES..].join("\n"), true)
    } else {
        (lines.join("\n"), false)
    };

    if kept.len() > MAX_OUTPUT_BYTES {
        let mut start = kept.len() - MAX_OUTPUT_BYTES;
        while start < kept.len() && !kept.is_char_boundary(start) {
            start += 1;
        }
        kept = kept[start..].to_owned();
        truncated = true;
    }

    if truncated {
        format!("[... earlier output tail-truncated ...]\n{kept}")
    } else {
        kept
    }
}

/// Normalises verification output into a one-line pass/fail summary using the
/// shared [`smedja_testkit`] parsers, picking whichever known format recognised
/// the most tests. Returns `None` when no format matched (e.g. a build error),
/// leaving the raw-output sections to carry the detail.
fn normalized_test_summary(stdout: &str, stderr: &str) -> Option<String> {
    let combined = format!("{stdout}\n{stderr}");
    let candidates = [
        smedja_testkit::parse::parse_cargo_text(&combined),
        smedja_testkit::parse::parse_go_json(&combined),
        smedja_testkit::parse::parse_junit_xml(&combined),
    ];
    let best = candidates
        .into_iter()
        .max_by_key(|p| p.passed + p.failed + p.skipped)?;
    if best.passed + best.failed + best.skipped == 0 {
        return None;
    }
    Some(format!(
        "{} passed, {} failed, {} skipped",
        best.passed, best.failed, best.skipped
    ))
}

/// Extracts recognisable failing test names from verification output.
///
/// Handles the two most common shapes: cargo/libtest lines
/// (`test module::name ... FAILED`) and pytest/generic lines
/// (`FAILED path::test`). Results are de-duplicated and capped at
/// [`MAX_FAILING_TESTS`].
fn failing_test_names(stdout: &str, stderr: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in stdout.lines().chain(stderr.lines()) {
        let trimmed = line.trim();
        if let Some(name) = trimmed
            .strip_prefix("test ")
            .and_then(|rest| rest.strip_suffix(" ... FAILED"))
        {
            let name = name.trim();
            if !name.is_empty() && !names.iter().any(|n| n == name) {
                names.push(name.to_owned());
            }
        } else if let Some(rest) = trimmed.strip_prefix("FAILED ") {
            let name = rest.split_whitespace().next().unwrap_or("").trim();
            if !name.is_empty() && !names.iter().any(|n| n == name) {
                names.push(name.to_owned());
            }
        }
        if names.len() >= MAX_FAILING_TESTS {
            break;
        }
    }
    names
}

/// Reads the failure guide for `role` from `.smedja/guides/<role>.md` inside
/// `workspace`, returning the raw Markdown content.
///
/// Returns `Ok(None)` when no guide exists for the role yet.
///
/// # Errors
///
/// Returns an error only on unexpected I/O failures (not on file-not-found).
pub fn read_failure_guide(role: &str, workspace: &Path) -> Result<Option<String>> {
    let path = workspace
        .join(".smedja")
        .join("guides")
        .join(format!("{role}.md"));

    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn failing(stdout: &str, stderr: &str) -> VerifyResult {
        VerifyResult {
            exit_code: 101,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            timed_out: false,
        }
    }

    #[test]
    fn normalized_summary_uses_testkit_cargo_parse() {
        let stdout = "test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s";
        let summary = normalized_test_summary(stdout, "").unwrap();
        assert_eq!(summary, "2 passed, 1 failed, 0 skipped");
    }

    #[test]
    fn normalized_summary_none_for_build_error() {
        let stderr = "error[E0432]: unresolved import `foo`";
        assert!(normalized_test_summary("", stderr).is_none());
    }

    #[test]
    fn verification_section_includes_normalized_report() {
        let result = failing(
            "test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.1s",
            "",
        );
        let section = render_verification(&result);
        assert!(section.contains("Normalised test report: 5 passed, 0 failed, 0 skipped"));
    }

    #[test]
    fn write_failure_guide_creates_file() {
        let dir = TempDir::new().unwrap();
        let patterns = vec!["forgot to run tests".to_owned(), "wrong branch".to_owned()];

        write_failure_guide("implementer", &patterns, None, dir.path()).unwrap();

        let path = dir
            .path()
            .join(".smedja")
            .join("guides")
            .join("implementer.md");
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("# Failure guide for `implementer`"));
        assert!(content.contains("- forgot to run tests"));
        assert!(content.contains("- wrong branch"));
    }

    #[test]
    fn write_failure_guide_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        // .smedja/guides should not exist yet.
        assert!(!dir.path().join(".smedja").exists());
        write_failure_guide("reviewer", &[], None, dir.path()).unwrap();
        assert!(dir
            .path()
            .join(".smedja")
            .join("guides")
            .join("reviewer.md")
            .exists());
    }

    #[test]
    fn write_failure_guide_with_empty_patterns_succeeds() {
        let dir = TempDir::new().unwrap();
        write_failure_guide("tester", &[], None, dir.path()).unwrap();
        let path = dir.path().join(".smedja").join("guides").join("tester.md");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("## Patterns"));
    }

    #[test]
    fn read_failure_guide_returns_none_when_missing() {
        let dir = TempDir::new().unwrap();
        let result = read_failure_guide("nonexistent", dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn read_failure_guide_returns_written_content() {
        let dir = TempDir::new().unwrap();
        let patterns = vec!["pattern one".to_owned()];
        write_failure_guide("fix", &patterns, None, dir.path()).unwrap();

        let content = read_failure_guide("fix", dir.path()).unwrap();
        assert!(content.is_some());
        assert!(content.unwrap().contains("pattern one"));
    }

    #[test]
    fn write_failure_guide_overwrites_existing_file() {
        let dir = TempDir::new().unwrap();
        let p1 = vec!["old pattern".to_owned()];
        let p2 = vec!["new pattern".to_owned()];

        write_failure_guide("proposer", &p1, None, dir.path()).unwrap();
        write_failure_guide("proposer", &p2, None, dir.path()).unwrap();

        let content = read_failure_guide("proposer", dir.path()).unwrap().unwrap();
        assert!(content.contains("new pattern"));
        assert!(!content.contains("old pattern"));
    }

    #[test]
    fn guide_embeds_real_verification_output() {
        let dir = TempDir::new().unwrap();
        let result = failing(
            "running 1 test\ntest widgets::rejects_negative ... FAILED\n\nfailures:\n\n\
             ---- widgets::rejects_negative stdout ----\nassertion `left == right` failed\n  \
             left: -1\n right: 0\n",
            "error: test failed, to rerun pass `-p widgets`\n",
        );

        write_failure_guide(
            "fix",
            &["slice failed".to_owned()],
            Some(&result),
            dir.path(),
        )
        .unwrap();

        let content = read_failure_guide("fix", dir.path()).unwrap().unwrap();
        // Still carries the metadata pattern...
        assert!(content.contains("slice failed"));
        // ...but now also the actual, distinctive failing output.
        assert!(content.contains("## Verification output"));
        assert!(content.contains("assertion `left == right` failed"));
        assert!(content.contains("exited with code 101"));
        // Failing test name surfaced near the top.
        assert!(content.contains("### Failing tests"));
        assert!(content.contains("widgets::rejects_negative"));
    }

    #[test]
    fn guide_tail_truncates_large_output() {
        let dir = TempDir::new().unwrap();
        // Distinctive first and last lines around a huge body.
        let mut body = String::from("FIRST_LINE_MARKER\n");
        for i in 0..10_000 {
            body.push_str(&format!("noise line {i} padding padding padding padding\n"));
        }
        body.push_str("LAST_LINE_MARKER\n");
        let result = failing(&body, "");

        write_failure_guide("fix", &[], Some(&result), dir.path()).unwrap();

        let content = read_failure_guide("fix", dir.path()).unwrap().unwrap();
        // The tail is retained; the head is dropped with a marker.
        assert!(content.contains("LAST_LINE_MARKER"));
        assert!(!content.contains("FIRST_LINE_MARKER"));
        assert!(content.contains("tail-truncated"));
        // Bounded: guide stays comfortably under the raw output size.
        assert!(content.len() < body.len());
        assert!(content.len() < MAX_OUTPUT_BYTES + 4096);
    }

    #[test]
    fn guide_reports_timeout_explicitly() {
        let dir = TempDir::new().unwrap();
        let result = VerifyResult {
            exit_code: -1,
            stdout: String::new(),
            stderr: "timed out".into(),
            timed_out: true,
        };

        write_failure_guide(
            "fix",
            &["slice failed".to_owned()],
            Some(&result),
            dir.path(),
        )
        .unwrap();

        let content = read_failure_guide("fix", dir.path()).unwrap().unwrap();
        assert!(content.contains("## Verification output"));
        assert!(content.contains("TIMED OUT"));
    }

    #[test]
    fn guide_with_empty_output_stays_generic() {
        let dir = TempDir::new().unwrap();
        let result = failing("", "   \n  ");

        write_failure_guide(
            "fix",
            &["slice 2 (idx 1) failed verification after 3 attempt(s)".to_owned()],
            Some(&result),
            dir.path(),
        )
        .unwrap();

        let content = read_failure_guide("fix", dir.path()).unwrap().unwrap();
        // Generic message preserved; no empty output section is emitted.
        assert!(content.contains("failed verification after 3 attempt(s)"));
        assert!(!content.contains("## Verification output"));
    }
}
