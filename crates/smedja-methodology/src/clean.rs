use crate::types::{GateResult, MethodologyViolation};

const GATE: &str = "CleanGate";
/// The debug-output macro this gate blocks in production code. Written as a
/// verbatim literal so the gate matches exactly the construct it is meant to
/// detect (the previous `concat!`-split form could only match a runtime-
/// reconstructed string and left the test bed never exercising the real macro).
const PRINTLN_MARKER: &str = "println!";

/// Checks a diff for hard structural violations.
///
/// The gate fails when any added (`+` prefixed) line outside a
/// `#[cfg(test)]` block contains:
/// - `.unwrap()` or `.expect(`
/// - `println!(…)`
///
/// Lines inside a `#[cfg(test)]` block are exempt because those are test
/// helpers where panicking assertions are acceptable.
///
/// # Errors
///
/// Returns a [`MethodologyViolation`] describing the first detected violation.
pub fn check(diff: &str) -> GateResult {
    let mut in_test_block = false;

    for line in diff.lines() {
        // Track whether we enter a cfg(test) region on any diff line so that
        // the test-block exemption applies correctly regardless of `+`/` ` prefix.
        if line.contains("#[cfg(test)]") {
            in_test_block = true;
        }

        // Only inspect added lines (not `+++` unified diff headers).
        if !line.starts_with('+') || line.starts_with("+++") {
            continue;
        }
        let content = &line[1..];

        if in_test_block {
            continue;
        }

        if content.contains(".unwrap()") || content.contains(".expect(") {
            return Err(MethodologyViolation::new(
                GATE,
                "unwrap/expect on non-test code".to_owned(),
            ));
        }

        if content.contains(PRINTLN_MARKER) {
            return Err(MethodologyViolation::new(
                GATE,
                "println! in library code".to_owned(),
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_fails_on_unwrap_in_non_test() {
        let diff = "+    let val = something.unwrap();\n";
        let result = check(diff);
        assert!(result.is_err());
        let violation = result.unwrap_err();
        assert_eq!(violation.gate, GATE);
    }

    #[test]
    fn clean_fails_on_println() {
        // Exercise the real macro literal so a regression in the matcher fails here.
        let diff = "+    println!(\"debug output\");\n";
        let result = check(diff);
        assert!(result.is_err());
        let violation = result.unwrap_err();
        assert_eq!(violation.gate, GATE);
        assert_eq!(violation.message, "println! in library code");
    }

    #[test]
    fn marker_is_the_literal_macro() {
        // The marker must be the exact construct the gate claims to block, not a
        // runtime-reconstructed split that the test bed never exercises.
        assert_eq!(PRINTLN_MARKER, "println!");
    }

    #[test]
    fn clean_passes_unwrap_in_test() {
        // unwrap inside a #[cfg(test)] block should be allowed
        let diff = "\
+#[cfg(test)]\n\
+mod tests {\n\
+    #[test]\n\
+    fn it_works() {\n\
+        let x: Option<u32> = Some(1);\n\
+        assert_eq!(x.unwrap(), 1);\n\
+    }\n\
+}\n";
        assert!(check(diff).is_ok());
    }

    #[test]
    fn clean_passes_clean_diff() {
        let diff = "\
+fn compute(x: u32) -> u32 {\n\
+    x.saturating_add(1)\n\
+}\n";
        assert!(check(diff).is_ok());
    }
}
