//! Methodology-gate glue: maps a session's `mode` string onto the compile-time
//! `Mode` enum and runs the corresponding diff-analysis gate set against a
//! proposed file write.
//!
//! The `Mode` enum is the single source of truth for which gate runs — there is
//! no runtime gate registry that configuration could silently disable.

use smedja_methodology::{clean, ponytail, tdd, MethodologyViolation, Mode};

/// Parses a persisted session `mode` string into a [`Mode`].
///
/// Returns `None` for an unset or unrecognised mode (those sessions are ungated).
#[must_use]
pub(crate) fn parse_mode(mode: Option<&str>) -> Option<Mode> {
    match mode? {
        "tdd" => Some(Mode::Tdd),
        "ponytail" => Some(Mode::Ponytail),
        "spec" => Some(Mode::Spec),
        "clean" => Some(Mode::Clean),
        "sre" => Some(Mode::Sre),
        _ => None,
    }
}

/// Runs the gate set for `mode` against `diff`, returning the first violation.
///
/// `Tdd` runs the TDD gate; `Clean` and `Ponytail` run the clean-code gate (the
/// ponytail gate shares the same matcher). `Spec` is enforced by the spec-first
/// lifecycle rather than a diff gate, and `Sre` is non-gating — both return
/// `None` here. The match is exhaustive, so a future `Mode` variant forces a
/// decision about its gate at compile time.
#[must_use]
pub(crate) fn run_gates(mode: &Mode, diff: &str) -> Option<MethodologyViolation> {
    let result = match mode {
        Mode::Tdd => tdd::check(diff),
        Mode::Clean => clean::check(diff),
        Mode::Ponytail => ponytail::check(diff),
        Mode::Spec | Mode::Sre => Ok(()),
    };
    result.err()
}

/// Builds a minimal unified-diff view of a proposed write for the gates.
///
/// The gates only inspect `+`-prefixed added lines, so the returned text marks
/// as added exactly the lines that are new relative to `current` (every line for
/// a new file). Context lines are left unmarked, which prevents re-flagging a
/// pre-existing `.unwrap()` / `println!` the write did not introduce.
#[must_use]
pub(crate) fn build_added_diff(current: &str, proposed: &str) -> String {
    let existing: std::collections::HashSet<&str> = current.lines().collect();
    let mut out = String::new();
    for line in proposed.lines() {
        if existing.contains(line) {
            // Unchanged context — leave unmarked so the gate ignores it.
            out.push(' ');
        } else {
            out.push('+');
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mode_known_and_unknown() {
        assert_eq!(parse_mode(Some("tdd")), Some(Mode::Tdd));
        assert_eq!(parse_mode(Some("ponytail")), Some(Mode::Ponytail));
        assert_eq!(parse_mode(Some("spec")), Some(Mode::Spec));
        assert_eq!(parse_mode(Some("clean")), Some(Mode::Clean));
        assert_eq!(parse_mode(Some("sre")), Some(Mode::Sre));
        assert_eq!(parse_mode(Some("interactive")), None);
        assert_eq!(parse_mode(None), None);
    }

    #[test]
    fn clean_gate_blocks_unwrap_in_added_lines() {
        let diff = build_added_diff("", "fn f() {\n    x.unwrap()\n}\n");
        let violation = run_gates(&Mode::Clean, &diff);
        assert!(violation.is_some());
    }

    #[test]
    fn clean_gate_ignores_preexisting_unwrap() {
        // The unwrap line already exists in current; the write only adds a comment.
        let current = "fn f() {\n    x.unwrap()\n}\n";
        let proposed = "fn f() {\n    // note\n    x.unwrap()\n}\n";
        let diff = build_added_diff(current, proposed);
        assert!(
            run_gates(&Mode::Clean, &diff).is_none(),
            "pre-existing unwrap must not be re-flagged"
        );
    }

    #[test]
    fn tdd_gate_blocks_impl_without_tests() {
        let diff = build_added_diff("", "fn new_feature() -> u32 { 42 }\n");
        assert!(run_gates(&Mode::Tdd, &diff).is_some());
    }

    #[test]
    fn spec_and_sre_modes_do_not_run_diff_gates() {
        let diff = build_added_diff("", "fn f() {\n    x.unwrap()\n}\n");
        assert!(run_gates(&Mode::Spec, &diff).is_none());
        assert!(run_gates(&Mode::Sre, &diff).is_none());
    }

    #[test]
    fn new_file_marks_all_lines_added() {
        let diff = build_added_diff("", "a\nb\n");
        assert_eq!(diff, "+a\n+b\n");
    }
}
