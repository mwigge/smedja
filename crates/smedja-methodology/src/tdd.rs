use crate::types::{GateResult, MethodologyViolation};

const GATE: &str = "TddGate";

/// Checks a diff for TDD compliance.
///
/// The gate passes when any of the following hold:
/// - The diff adds no new non-test `fn ` lines.
/// - The diff adds at least one `#[test]` or `mod tests` line alongside
///   any new implementation.
///
/// # Errors
///
/// Returns a [`MethodologyViolation`] when the diff adds implementation
/// (`+fn `) lines but no test annotations (`+#[test]` or `+mod tests`).
pub fn check(diff: &str) -> GateResult {
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

    if impl_lines > 0 && test_lines == 0 {
        return Err(MethodologyViolation::new(
            GATE,
            format!(
                "{impl_lines} implementation fn(s) added with no accompanying \
                 `#[test]` or `mod tests` lines"
            ),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tdd_passes_when_diff_has_test_and_impl() {
        let diff = "\
+fn foo() -> u32 { 42 }\n\
+#[test]\n\
+fn test_foo() { assert_eq!(foo(), 42); }\n";
        assert!(check(diff).is_ok());
    }

    #[test]
    fn tdd_fails_when_impl_without_test() {
        let diff = "\
+fn bar(x: u32) -> u32 { x + 1 }\n\
 // no test lines\n";
        let result = check(diff);
        assert!(result.is_err());
        let violation = result.unwrap_err();
        assert_eq!(violation.gate, GATE);
    }

    #[test]
    fn tdd_passes_empty_diff() {
        assert!(check("").is_ok());
    }

    #[test]
    fn tdd_passes_test_only_diff() {
        let diff = "\
+#[test]\n\
+fn test_only() { assert!(true); }\n";
        assert!(check(diff).is_ok());
    }
}
