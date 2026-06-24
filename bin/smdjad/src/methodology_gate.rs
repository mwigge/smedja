//! Methodology-gate glue: maps a session's `mode` string onto the compile-time
//! `Mode` enum and runs the corresponding diff-analysis gate set against a
//! proposed file write.
//!
//! The `Mode` enum is the single source of truth for which gate runs — there is
//! no runtime gate registry that configuration could silently disable.

use smedja_methodology::{clean, MethodologyViolation, Mode};

/// Parses a persisted session `mode` string into a [`Mode`].
///
/// Returns `None` for an unset or unrecognised mode (those sessions are ungated).
/// The strings `"tdd"`, `"ponytail"`, and `"sre"` now resolve to `None`: TDD and
/// clean-code are the always-on foundational discipline (not selectable modes),
/// and the dormant `sre` gate was retired. A stale persisted `"tdd"`/`"ponytail"`
/// /`"sre"` therefore degrades gracefully to ungated.
#[must_use]
pub(crate) fn parse_mode(mode: Option<&str>) -> Option<Mode> {
    match mode? {
        "spec" => Some(Mode::Spec),
        "clean" => Some(Mode::Clean),
        _ => None,
    }
}

/// Runs the gate set for `mode` against `diff`, returning the first violation.
///
/// `Clean` runs the hard clean-code backstop. `Spec` is enforced by the
/// spec-first lifecycle rather than a diff gate, so it returns `None` here. The
/// match is exhaustive, so a future `Mode` variant forces a decision about its
/// gate at compile time. The TDD discipline is enforced foundationally (steering
/// every turn plus an advisory backstop), not through this selectable path.
#[must_use]
pub(crate) fn run_gates(mode: &Mode, diff: &str) -> Option<MethodologyViolation> {
    let result = match mode {
        Mode::Clean => clean::check(diff),
        Mode::Spec => Ok(()),
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
    fn parse_mode_retained_modes_resolve() {
        assert_eq!(parse_mode(Some("spec")), Some(Mode::Spec));
        assert_eq!(parse_mode(Some("clean")), Some(Mode::Clean));
    }

    #[test]
    fn parse_mode_removed_and_unknown_resolve_to_none() {
        // The removed selectable modes now degrade gracefully to ungated.
        assert_eq!(parse_mode(Some("tdd")), None);
        assert_eq!(parse_mode(Some("ponytail")), None);
        assert_eq!(parse_mode(Some("sre")), None);
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
    fn spec_mode_does_not_run_diff_gates() {
        let diff = build_added_diff("", "fn f() {\n    x.unwrap()\n}\n");
        assert!(run_gates(&Mode::Spec, &diff).is_none());
    }

    #[test]
    fn new_file_marks_all_lines_added() {
        let diff = build_added_diff("", "a\nb\n");
        assert_eq!(diff, "+a\n+b\n");
    }
}
