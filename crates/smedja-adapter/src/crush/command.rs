//! Task 52 — RTK-style command-aware output compressor.

use super::bypass_enabled;
use super::registry::FilterRegistry;

/// Compresses command output by dispatching through the default
/// [`FilterRegistry`] keyed on the detected command.
///
/// Returns `(compressed_output, ratio)` where `ratio = compressed.len() as f32 /
/// output.len() as f32`.  A ratio below 1.0 means the output was reduced.
///
/// The strategy is selected by [`FilterRegistry::with_defaults`] from the first
/// one or two tokens of `cmd`.  The default set preserves the historical
/// `cargo test` (smart-filter) and `git status` (group) behaviour; an
/// unrecognised command falls back to the conservative blank-line removal.
///
/// `SMEDJA_NO_TOOL_COMPRESS=1` bypasses all processing and returns the output
/// verbatim with ratio `1.0`.
#[must_use]
pub fn compress_command_output(cmd: &str, output: &str) -> (String, f32) {
    compress_command_output_with(&FilterRegistry::with_defaults(), cmd, output)
}

/// Compresses command output using an explicit `registry`.
///
/// This is the registry-aware core of [`compress_command_output`]; callers that
/// have loaded a user `.smedja/filters.toml` registry route through here so the
/// merged user/default filter set is applied.  The bypass env var and the
/// empty-output shortcut are honoured identically.
///
/// Returns `(compressed_output, ratio)`.
#[must_use]
pub fn compress_command_output_with(
    registry: &FilterRegistry,
    cmd: &str,
    output: &str,
) -> (String, f32) {
    if bypass_enabled() {
        return (output.to_owned(), 1.0_f32);
    }

    if output.is_empty() {
        return (String::new(), 1.0_f32);
    }

    let (strategy, params) = registry.resolve(cmd);
    let compressed = strategy.apply(output, &params);

    #[allow(clippy::cast_precision_loss)] // advisory ratio; precision loss is acceptable
    let ratio = compressed.len() as f32 / output.len() as f32;
    (compressed, ratio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_test_noisy_output_compressed_below_threshold() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let noisy = "\
running 42 tests\n\
test result: ok. 42 passed; 0 failed; 0 ignored; 0 measured\n\
\n\
running 1 test\n\
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured\n\
FAILED: some_test at src/lib.rs:10\n\
error[E0001]: something went wrong\n\
  --> src/lib.rs:10:5\n\
   |\n\
10 |     let x = undefined;\n\
   |             ^^^^^^^^^\n\
\n";
        let (compressed, ratio) = compress_command_output("cargo test", noisy);
        assert!(
            ratio <= 0.80_f32,
            "expected compression ratio ≤ 0.80, got {ratio:.3}"
        );
        // The actual error line must survive.
        assert!(
            compressed.contains("error[E0001]"),
            "error lines must be preserved"
        );
    }

    #[test]
    fn git_status_boilerplate_removed() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let input = "On branch main\n\
Your branch is up to date with 'origin/main'.\n\
\n\
Changes not staged for commit:\n\
\t modified: src/lib.rs\n";
        let (compressed, _) = compress_command_output("git status", input);
        assert!(
            !compressed.contains("On branch"),
            "boilerplate must be removed"
        );
        assert!(
            compressed.contains("src/lib.rs"),
            "changed file must be preserved"
        );
    }

    #[test]
    fn unknown_command_passthrough_except_blanks() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let input = "line one\n\nline two\n\nline three\n";
        let (compressed, _) = compress_command_output("grep pattern file.txt", input);
        assert!(!compressed.contains("\n\n"), "blank lines must be removed");
        assert!(compressed.contains("line one"));
        assert!(compressed.contains("line three"));
    }

    #[test]
    fn bypass_env_skips_command_compression() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        std::env::set_var("SMEDJA_NO_TOOL_COMPRESS", "1");
        let input = "running 1 tests\ntest result: ok. 1 passed\n";
        let (compressed, ratio) = compress_command_output("cargo test", input);
        std::env::remove_var("SMEDJA_NO_TOOL_COMPRESS");
        assert_eq!(compressed, input);
        assert!((ratio - 1.0_f32).abs() < f32::EPSILON);
    }

    #[test]
    fn ratio_below_one_for_compressed_output() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let noisy = "running 100 tests\n".repeat(20)
            + "error[E0001]: the actual error\n  --> src/lib.rs:1:1\n";
        let (_, ratio) = compress_command_output("cargo test", &noisy);
        assert!(
            ratio < 1.0_f32,
            "compression of noisy output must yield ratio < 1.0, got {ratio:.3}"
        );
    }
}
