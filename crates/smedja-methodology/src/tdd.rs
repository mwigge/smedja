use crate::types::MethodologyViolation;

const GATE: &str = "TddGate";

/// The number of new implementation `fn` lines a test-free change may add before
/// the backstop considers it "substantial".
///
/// Below this threshold a test-free change is treated as a refactor, helper
/// extraction, or doc edit and is *not* flagged — the steering directive carries
/// the discipline for those. At or above it, the change looks like a genuine
/// feature landed without tests, which the advisory backstop surfaces.
const SUBSTANTIAL_IMPL_THRESHOLD: u32 = 3;

/// The outcome of the foundational TDD backstop over a diff.
///
/// The backstop is *advisory*: even when it raises, the value is the steering
/// directive being present on every turn, not a blunt reject. Callers decide how
/// to surface an advisory verdict (typically a warning, not a block).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TddVerdict {
    /// The change is within the foundational tolerance (no test-free *substantial*
    /// implementation): a refactor, helper, doc edit, or a change that already
    /// carries tests.
    Pass,
    /// The change adds substantial new implementation with zero tests anywhere in
    /// the diff. This is advisory, not a hard block.
    Advisory(MethodologyViolation),
}

impl TddVerdict {
    /// Returns `true` when the verdict is an advisory finding.
    #[must_use]
    pub fn is_advisory(&self) -> bool {
        matches!(self, TddVerdict::Advisory(_))
    }
}

/// Evaluates a diff against the foundational TDD backstop.
///
/// This is the relaxed, always-on backstop: it is *not* a naive hard-block on
/// every test-free `fn`. As an always-on foundation, that crude rule would fight
/// refactors, helper extraction, and doc edits — so the backstop raises only when
/// a change adds *substantial* new implementation
/// ([`SUBSTANTIAL_IMPL_THRESHOLD`] or more new `fn` lines) with *zero* test
/// annotations (`#[test]` / `mod tests`) anywhere in the diff. The result is
/// advisory: the steering directive present on every turn is the primary
/// enforcement.
#[must_use]
pub fn evaluate(diff: &str) -> TddVerdict {
    let mut impl_lines: u32 = 0;
    let mut test_lines: u32 = 0;

    for line in diff.lines() {
        // Only inspect added lines (unified diff `+` prefix, not `+++` header).
        if !line.starts_with('+') || line.starts_with("+++") {
            continue;
        }
        let content = &line[1..];

        if content.contains("#[test]") || content.contains("mod tests") {
            test_lines += 1;
        } else if content.contains("fn ") {
            impl_lines += 1;
        }
    }

    if test_lines == 0 && impl_lines >= SUBSTANTIAL_IMPL_THRESHOLD {
        return TddVerdict::Advisory(MethodologyViolation::new(
            GATE,
            format!(
                "{impl_lines} implementation fn(s) added with no accompanying \
                 `#[test]` or `mod tests` lines anywhere in the change"
            ),
        ));
    }

    TddVerdict::Pass
}

/// Backwards-compatible wrapper exposing the backstop as a `GateResult`.
///
/// Returns `Ok(())` for [`TddVerdict::Pass`] and `Err(..)` for an advisory
/// verdict. Callers that need to keep the TDD backstop *advisory* (not blocking)
/// should call [`evaluate`] directly and inspect [`TddVerdict`]; this wrapper
/// exists for call sites that only care whether the egregious case was hit.
///
/// # Errors
///
/// Returns the advisory [`MethodologyViolation`] when [`evaluate`] returns
/// [`TddVerdict::Advisory`].
pub fn check(diff: &str) -> crate::types::GateResult {
    match evaluate(diff) {
        TddVerdict::Pass => Ok(()),
        TddVerdict::Advisory(v) => Err(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refactor_adding_single_fn_without_test_is_not_flagged() {
        // The false positive the old naive rule produced: a refactor / helper
        // extraction that adds one impl `fn` with no co-located test must pass.
        let diff = "\
+fn helper(x: u32) -> u32 { x + 1 }\n\
 // no test lines\n";
        assert_eq!(evaluate(diff), TddVerdict::Pass);
        assert!(check(diff).is_ok());
    }

    #[test]
    fn small_test_free_change_below_threshold_is_not_flagged() {
        let diff = "\
+fn a() {}\n\
+fn b() {}\n";
        assert_eq!(evaluate(diff), TddVerdict::Pass);
    }

    #[test]
    fn substantial_impl_without_tests_raises_advisory() {
        let diff = "\
+fn one() -> u32 { 1 }\n\
+fn two() -> u32 { 2 }\n\
+fn three() -> u32 { 3 }\n\
+fn four() -> u32 { 4 }\n";
        let verdict = evaluate(diff);
        assert!(verdict.is_advisory());
        match verdict {
            TddVerdict::Advisory(v) => assert_eq!(v.gate, GATE),
            TddVerdict::Pass => panic!("expected advisory"),
        }
    }

    #[test]
    fn substantial_impl_with_a_test_passes() {
        let diff = "\
+fn one() -> u32 { 1 }\n\
+fn two() -> u32 { 2 }\n\
+fn three() -> u32 { 3 }\n\
+#[test]\n\
+fn test_one() { assert_eq!(one(), 1); }\n";
        assert_eq!(evaluate(diff), TddVerdict::Pass);
    }

    #[test]
    fn tdd_passes_empty_diff() {
        assert_eq!(evaluate(""), TddVerdict::Pass);
        assert!(check("").is_ok());
    }

    #[test]
    fn tdd_passes_test_only_diff() {
        let diff = "\
+#[test]\n\
+fn test_only() { assert!(true); }\n";
        assert_eq!(evaluate(diff), TddVerdict::Pass);
    }
}
