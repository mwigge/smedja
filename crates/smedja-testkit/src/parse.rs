//! Framework-output parsers that normalise into a single [`Parsed`] tally.
//!
//! Each `parse_*` function is pure: it takes captured stdout/stderr (or an XML
//! document) and returns counts plus the individual failures. [`crate::run`]
//! adds the runner label and wall-clock duration to build a
//! [`crate::SuiteReport`]. Keeping the parsers pure makes them directly
//! testable against real framework output samples without spawning processes.

use crate::Failure;

/// A framework-agnostic tally of one suite's results.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    /// Passing tests.
    pub passed: u32,
    /// Failing tests.
    pub failed: u32,
    /// Skipped/ignored tests.
    pub skipped: u32,
    /// Individual failures with names and messages.
    pub failures: Vec<Failure>,
    /// Optional duration in milliseconds parsed from the output, when present.
    pub duration_ms: Option<u64>,
}

// ── Rust: `cargo test` human output ──────────────────────────────────────────

/// Parses `cargo test` (libtest) human output.
///
/// Sums every `test result: ok. N passed; M failed; K ignored; …` line (there
/// is one per test binary) and collects failing test names from the trailing
/// `failures:` block, pairing each with the panic line from its
/// `---- <name> stdout ----` section when available.
#[must_use]
pub fn parse_cargo_text(output: &str) -> Parsed {
    let mut parsed = Parsed::default();
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("test result:") {
            parsed.passed += field_before(rest, "passed");
            parsed.failed += field_before(rest, "failed");
            parsed.skipped += field_before(rest, "ignored");
        }
    }
    parsed.failures = cargo_failures(output);
    // If the summary omitted a per-test count but named failures, trust names.
    if parsed.failed == 0 && !parsed.failures.is_empty() {
        parsed.failed = u32::try_from(parsed.failures.len()).unwrap_or(u32::MAX);
    }
    parsed
}

/// Reads the integer immediately preceding `keyword` in a `cargo` summary tail,
/// e.g. `field_before(" 3 passed; 1 failed", "passed") == 3`.
fn field_before(text: &str, keyword: &str) -> u32 {
    let Some(idx) = text.find(keyword) else {
        return 0;
    };
    text[..idx]
        .split_whitespace()
        .last()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Extracts failing test names + panic messages from a `cargo test` transcript.
fn cargo_failures(output: &str) -> Vec<Failure> {
    let mut failures = Vec::new();
    let lines: Vec<&str> = output.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        // Section header: `---- tests::foo stdout ----`
        let Some(name) = trimmed
            .strip_prefix("---- ")
            .and_then(|r| r.strip_suffix(" stdout ----"))
        else {
            continue;
        };
        // Message: first `thread '…' panicked at …` after the header.
        let message = lines[i + 1..]
            .iter()
            .take(10)
            .map(|l| l.trim())
            .find(|l| l.starts_with("thread '") || l.contains("panicked at"))
            .unwrap_or("")
            .to_owned();
        failures.push(Failure {
            name: name.to_owned(),
            message,
        });
    }
    failures
}

// ── Rust: libtest / nextest JSON (`--message-format libtest-json`) ───────────

/// Parses libtest / `cargo nextest` JSON output — one JSON object per line with
/// `{ "type": "test", "name": …, "event": "ok"|"failed"|"ignored" }`.
#[must_use]
pub fn parse_libtest_json(output: &str) -> Parsed {
    let mut parsed = Parsed::default();
    for line in output.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("test") {
            continue;
        }
        match v.get("event").and_then(|e| e.as_str()) {
            Some("ok") => parsed.passed += 1,
            Some("ignored") => parsed.skipped += 1,
            Some("failed") => {
                parsed.failed += 1;
                parsed.failures.push(Failure {
                    name: v
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_owned(),
                    message: v
                        .get("stdout")
                        .and_then(|s| s.as_str())
                        .map(first_meaningful_line)
                        .unwrap_or_default(),
                });
            }
            _ => {}
        }
    }
    parsed
}

// ── Go: `go test -json` ──────────────────────────────────────────────────────

/// Parses `go test -json` output — a stream of JSON objects with an `Action`
/// (`pass`/`fail`/`skip`) at test granularity (those carrying a `Test` field).
#[must_use]
pub fn parse_go_json(output: &str) -> Parsed {
    use std::collections::HashMap;
    let mut parsed = Parsed::default();
    let mut out_by_test: HashMap<String, String> = HashMap::new();

    for line in output.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(test) = v.get("Test").and_then(|t| t.as_str()) else {
            continue; // package-level event
        };
        match v.get("Action").and_then(|a| a.as_str()) {
            Some("output") => {
                if let Some(o) = v.get("Output").and_then(|o| o.as_str()) {
                    out_by_test.entry(test.to_owned()).or_default().push_str(o);
                }
            }
            Some("pass") => parsed.passed += 1,
            Some("skip") => parsed.skipped += 1,
            Some("fail") => {
                parsed.failed += 1;
                let message = out_by_test
                    .get(test)
                    .map(|s| first_meaningful_line(s))
                    .unwrap_or_default();
                parsed.failures.push(Failure {
                    name: test.to_owned(),
                    message,
                });
            }
            _ => {}
        }
    }
    parsed
}

// ── Jest: `jest --json` ──────────────────────────────────────────────────────

/// Parses `jest --json` output (a single JSON document with top-level
/// `numPassedTests` / `numFailedTests` / `numPendingTests` and per-assertion
/// `testResults`).
#[must_use]
pub fn parse_jest_json(output: &str) -> Parsed {
    let Some(start) = output.find('{') else {
        return Parsed::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&output[start..]) else {
        return Parsed::default();
    };
    let as_u32 = |k: &str| -> u32 {
        u32::try_from(v.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0)).unwrap_or(u32::MAX)
    };
    let mut parsed = Parsed {
        passed: as_u32("numPassedTests"),
        failed: as_u32("numFailedTests"),
        skipped: as_u32("numPendingTests") + as_u32("numTodoTests"),
        ..Parsed::default()
    };
    if let Some(files) = v.get("testResults").and_then(|t| t.as_array()) {
        for file in files {
            let Some(assertions) = file.get("assertionResults").and_then(|a| a.as_array()) else {
                continue;
            };
            for a in assertions {
                if a.get("status").and_then(|s| s.as_str()) == Some("failed") {
                    parsed.failures.push(Failure {
                        name: a
                            .get("fullName")
                            .or_else(|| a.get("title"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_owned(),
                        message: a
                            .get("failureMessages")
                            .and_then(|m| m.as_array())
                            .and_then(|arr| arr.first())
                            .and_then(|s| s.as_str())
                            .map(first_meaningful_line)
                            .unwrap_or_default(),
                    });
                }
            }
        }
    }
    parsed
}

// ── Generic JUnit XML fallback (pytest, gradle, maven, dotnet, …) ────────────

/// Parses a JUnit-XML document into a [`Parsed`] tally.
///
/// This is the lenient cross-framework fallback: it scans `<testcase …>`
/// elements (whether self-closing or with a body) and classifies each by the
/// presence of a `<failure>`, `<error>`, or `<skipped>` child. It does not
/// require a real XML parser — the scan tolerates attribute order, namespaces,
/// and surrounding prose.
#[must_use]
pub fn parse_junit_xml(xml: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let bytes = xml.as_bytes();
    let mut cursor = 0usize;

    while let Some(rel) = xml[cursor..].find("<testcase") {
        let open = cursor + rel;
        // Body ends at the matching close: either the self-closing `/>` or the
        // next `</testcase>`. Whichever comes first bounds this element.
        let after_open = open + "<testcase".len();
        let self_close = find_from(xml, after_open, "/>");
        let paired_close = find_from(xml, after_open, "</testcase>");
        let (body_end, next_cursor) = match (self_close, paired_close) {
            (Some(sc), Some(pc)) if sc < pc => (sc, sc + 2),
            (Some(sc), None) => (sc, sc + 2),
            (_, Some(pc)) => (pc, pc + "</testcase>".len()),
            (None, None) => break,
        };
        let element = &xml[open..body_end];
        let name = attr(element, "name").unwrap_or_default();

        if element.contains("<skipped") {
            parsed.skipped += 1;
        } else if element.contains("<failure") || element.contains("<error") {
            parsed.failed += 1;
            let message = failure_message(element);
            parsed.failures.push(Failure { name, message });
        } else {
            parsed.passed += 1;
        }
        cursor = next_cursor.min(bytes.len());
    }
    parsed
}

/// Extracts the `message` attribute of a `<failure>`/`<error>` child, else the
/// first meaningful line of its text body.
fn failure_message(element: &str) -> String {
    for tag in ["<failure", "<error"] {
        if let Some(idx) = element.find(tag) {
            let tail = &element[idx..];
            if let Some(msg) = attr(tail, "message") {
                if !msg.is_empty() {
                    return msg;
                }
            }
            // Text between `>` and the closing tag.
            if let Some(gt) = tail.find('>') {
                let body = &tail[gt + 1..];
                let end = body.find("</").unwrap_or(body.len());
                let text = first_meaningful_line(&body[..end]);
                if !text.is_empty() {
                    return text;
                }
            }
        }
    }
    String::new()
}

// ── Lenient text summaries (npm/mocha, when no machine format is available) ───

/// Best-effort parse of plain test-runner console output when no structured
/// format was produced. Recognises jest's `Tests: … passed, … failed` summary
/// and mocha's `N passing` / `N failing` lines.
#[must_use]
pub fn parse_lenient_text(output: &str) -> Parsed {
    let mut parsed = Parsed::default();
    for line in output.lines() {
        let l = line.trim();
        // jest: "Tests:       1 failed, 5 passed, 6 total"
        if let Some(rest) = l.strip_prefix("Tests:") {
            parsed.passed = parsed.passed.max(field_after(rest, "passed"));
            parsed.failed = parsed.failed.max(field_after(rest, "failed"));
            parsed.skipped = parsed.skipped.max(field_after(rest, "skipped"));
        }
        // mocha: "  5 passing" / "  1 failing" / "  2 pending"
        if let Some(n) = trailing_count(l, "passing") {
            parsed.passed = parsed.passed.max(n);
        }
        if let Some(n) = trailing_count(l, "failing") {
            parsed.failed = parsed.failed.max(n);
        }
        if let Some(n) = trailing_count(l, "pending") {
            parsed.skipped = parsed.skipped.max(n);
        }
    }
    parsed
}

/// Reads the integer that follows `keyword` where the token order is
/// `<N> <keyword>` but keyword precedes the value's comma group, e.g. jest's
/// `"1 failed, 5 passed"`. We scan tokens for `<N> keyword`.
fn field_after(text: &str, keyword: &str) -> u32 {
    let tokens: Vec<&str> = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .collect();
    for pair in tokens.windows(2) {
        if pair[1] == keyword {
            if let Ok(n) = pair[0].parse() {
                return n;
            }
        }
    }
    0
}

/// Reads `"  5 passing"` style: the integer token immediately before `keyword`.
fn trailing_count(line: &str, keyword: &str) -> Option<u32> {
    let mut toks = line.split_whitespace();
    let mut prev: Option<&str> = None;
    for t in toks.by_ref() {
        if t == keyword {
            return prev.and_then(|p| p.parse().ok());
        }
        prev = Some(t);
    }
    None
}

// ── shared helpers ───────────────────────────────────────────────────────────

/// The first non-empty, trimmed line of `text` (used to compact multi-line
/// failure bodies into a single message).
fn first_meaningful_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// Finds `needle` in `hay` starting at byte `from`, returning an absolute index.
fn find_from(hay: &str, from: usize, needle: &str) -> Option<usize> {
    hay.get(from..)
        .and_then(|s| s.find(needle))
        .map(|i| i + from)
}

/// Extracts the value of XML/HTML attribute `key` from `element` (double or
/// single quoted).
fn attr(element: &str, key: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let pat = format!("{key}={quote}");
        let mut from = 0usize;
        while let Some(rel) = element[from..].find(&pat) {
            let idx = from + rel;
            // Require an attribute boundary before `key` so `name=` does not
            // match inside `classname=`.
            let boundary = idx == 0
                || !element[..idx]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            let start = idx + pat.len();
            if boundary {
                if let Some(end) = element[start..].find(quote) {
                    return Some(xml_unescape(&element[start..start + end]));
                }
            }
            from = start;
        }
    }
    None
}

/// Minimal XML entity unescape for attribute values.
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_text_sums_binaries_and_names_failures() {
        let out = "\
running 3 tests
test tests::a ... ok
test tests::b ... FAILED
test tests::c ... ok

failures:

---- tests::b stdout ----
thread 'tests::b' panicked at src/lib.rs:10:5:
assertion failed

failures:
    tests::b

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
test result: ok. 5 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.02s
";
        let p = parse_cargo_text(out);
        assert_eq!(p.passed, 7);
        assert_eq!(p.failed, 1);
        assert_eq!(p.skipped, 1);
        assert_eq!(p.failures.len(), 1);
        assert_eq!(p.failures[0].name, "tests::b");
        assert!(p.failures[0].message.contains("panicked"));
    }

    #[test]
    fn libtest_json_counts_events() {
        let out = "\
{ \"type\": \"suite\", \"event\": \"started\", \"test_count\": 3 }
{ \"type\": \"test\", \"event\": \"started\", \"name\": \"a\" }
{ \"type\": \"test\", \"name\": \"a\", \"event\": \"ok\" }
{ \"type\": \"test\", \"name\": \"b\", \"event\": \"failed\", \"stdout\": \"boom\\nline2\" }
{ \"type\": \"test\", \"name\": \"c\", \"event\": \"ignored\" }
";
        let p = parse_libtest_json(out);
        assert_eq!(p.passed, 1);
        assert_eq!(p.failed, 1);
        assert_eq!(p.skipped, 1);
        assert_eq!(p.failures[0].name, "b");
        assert_eq!(p.failures[0].message, "boom");
    }

    #[test]
    fn go_json_classifies_and_collects_output() {
        let out = "\
{\"Action\":\"run\",\"Test\":\"TestA\"}
{\"Action\":\"output\",\"Test\":\"TestA\",\"Output\":\"ok\\n\"}
{\"Action\":\"pass\",\"Test\":\"TestA\"}
{\"Action\":\"output\",\"Test\":\"TestB\",\"Output\":\"    foo_test.go:9: not equal\\n\"}
{\"Action\":\"fail\",\"Test\":\"TestB\"}
{\"Action\":\"skip\",\"Test\":\"TestC\"}
{\"Action\":\"pass\",\"Package\":\"pkg\"}
";
        let p = parse_go_json(out);
        assert_eq!(p.passed, 1);
        assert_eq!(p.failed, 1);
        assert_eq!(p.skipped, 1);
        assert_eq!(p.failures[0].name, "TestB");
        assert!(p.failures[0].message.contains("not equal"));
    }

    #[test]
    fn jest_json_reads_counts_and_failures() {
        let out = r#"{"numPassedTests":5,"numFailedTests":1,"numPendingTests":2,
          "testResults":[{"assertionResults":[
            {"title":"adds","fullName":"math adds","status":"passed"},
            {"title":"subs","fullName":"math subs","status":"failed","failureMessages":["Expected 1\n  got 2"]}
          ]}]}"#;
        let p = parse_jest_json(out);
        assert_eq!(p.passed, 5);
        assert_eq!(p.failed, 1);
        assert_eq!(p.skipped, 2);
        assert_eq!(p.failures[0].name, "math subs");
        assert!(p.failures[0].message.contains("Expected 1"));
    }

    #[test]
    fn junit_xml_self_closing_and_paired() {
        let xml = r#"<testsuite tests="3" failures="1" skipped="1">
  <testcase classname="m" name="test_a" time="0.1"/>
  <testcase classname="m" name="test_b" time="0.2"><failure message="assert 1 == 2">trace</failure></testcase>
  <testcase classname="m" name="test_c"><skipped/></testcase>
</testsuite>"#;
        let p = parse_junit_xml(xml);
        assert_eq!(p.passed, 1);
        assert_eq!(p.failed, 1);
        assert_eq!(p.skipped, 1);
        assert_eq!(p.failures[0].name, "test_b");
        assert_eq!(p.failures[0].message, "assert 1 == 2");
    }

    #[test]
    fn junit_error_body_text_used_when_no_message_attr() {
        let xml = r#"<testcase name="boom"><error>NullPointerException at X</error></testcase>"#;
        let p = parse_junit_xml(xml);
        assert_eq!(p.failed, 1);
        assert_eq!(p.failures[0].message, "NullPointerException at X");
    }

    #[test]
    fn lenient_text_reads_jest_and_mocha() {
        let jest = "Tests:       1 failed, 5 passed, 6 total";
        let p = parse_lenient_text(jest);
        assert_eq!(p.passed, 5);
        assert_eq!(p.failed, 1);

        let mocha = "  5 passing (1s)\n  1 failing\n  2 pending";
        let p = parse_lenient_text(mocha);
        assert_eq!(p.passed, 5);
        assert_eq!(p.failed, 1);
        assert_eq!(p.skipped, 2);
    }

    #[test]
    fn xml_unescape_handles_entities() {
        assert_eq!(xml_unescape("a &lt;b&gt; &amp; c"), "a <b> & c");
    }
}
