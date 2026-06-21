use crate::{clean, types::GateResult};

const GATE: &str = "PonytailGate";

/// Checks a diff for over-engineering smells.
///
/// Scans added (`+` prefixed) lines for:
/// - `.unwrap()` or `.expect(` outside `#[cfg(test)]` blocks → hard violation
/// - `println!(…)` → hard violation
///
/// Advisory findings (e.g. `Box<dyn`) do not cause a violation; the gate
/// returns `Ok(())` for those.
///
/// # Errors
///
/// Returns a [`MethodologyViolation`] when a hard-blocking smell is detected.
pub fn check(diff: &str) -> GateResult {
    clean::check_added_lines(GATE, diff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ponytail_flags_println() {
        let diff = concat!("+    println", "!(\"x\");\n");
        let result = check(diff);
        assert!(result.is_err());
        let violation = result.unwrap_err();
        assert_eq!(violation.gate, GATE);
    }

    #[test]
    fn ponytail_flags_unwrap_outside_test() {
        let diff = "+    let v = result.unwrap();\n";
        let result = check(diff);
        assert!(result.is_err());
        let violation = result.unwrap_err();
        assert_eq!(violation.gate, GATE);
    }

    #[test]
    fn ponytail_passes_clean_diff() {
        let diff = "\
+fn tidy(x: u32) -> u32 {\n\
+    x.wrapping_mul(2)\n\
+}\n";
        assert!(check(diff).is_ok());
    }
}
