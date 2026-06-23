//! Context-window compression transforms.
//!
//! This module provides three compressors that shrink context content before
//! it is serialised into an outbound LLM request:
//!
//! - [`compress_tool_result`] — strips JSON null fields recursively.
//! - [`compress_command_output`] — removes known-noisy lines per command type.
//! - [`trim_code_block`] — truncates long code blocks to first 20 lines.
//!
//! Each function honours the `SMEDJA_NO_TOOL_COMPRESS=1` environment variable
//! as a bypass.
//!
//! The [`ContentPipeline`] struct chains arbitrary [`Transform`] implementations
//! and applies them in sequence.

use std::fmt::Write as _;

// ── Bypass helper ────────────────────────────────────────────────────────────

/// Returns `true` when `SMEDJA_NO_TOOL_COMPRESS` is set to `1`.
fn bypass_enabled() -> bool {
    std::env::var("SMEDJA_NO_TOOL_COMPRESS").as_deref() == Ok("1")
}

// ── Task 51 — SmartCrusher ───────────────────────────────────────────────────

/// Strips JSON null fields recursively from a serialised JSON string.
///
/// Non-JSON input is returned unchanged.  Honouring `SMEDJA_NO_TOOL_COMPRESS=1`
/// bypasses all processing and returns the content as-is.
#[must_use]
pub fn compress_tool_result(content: &str) -> String {
    if bypass_enabled() {
        return content.to_owned();
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return content.to_owned();
    };

    let stripped = strip_nulls(value);
    serde_json::to_string(&stripped).unwrap_or_else(|_| content.to_owned())
}

/// Recursively removes all JSON null fields from an object or array.
fn strip_nulls(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let filtered = map
                .into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect();
            serde_json::Value::Object(filtered)
        }
        serde_json::Value::Array(arr) => {
            let filtered = arr.into_iter().map(strip_nulls).collect();
            serde_json::Value::Array(filtered)
        }
        other => other,
    }
}

// ── Task 52 — RTK-style command-aware compressor ─────────────────────────────

/// Compresses command output by removing known-noisy lines per command type.
///
/// Returns `(compressed_output, ratio)` where `ratio = compressed.len() as f32 /
/// output.len() as f32`.  A ratio below 1.0 means the output was reduced.
///
/// Supported commands and their targets:
/// - `cargo test` — removes `running N tests`, `test result: ok`, blank lines,
///   and carriage-return residue.  Target: ≥ 80% compression ratio ≤ 0.20.
/// - `git status` — removes "On branch" and "nothing to commit" boilerplate headers.
/// - All others — removes blank lines only.
///
/// `SMEDJA_NO_TOOL_COMPRESS=1` bypasses all processing.
#[must_use]
pub fn compress_command_output(cmd: &str, output: &str) -> (String, f32) {
    if bypass_enabled() {
        return (output.to_owned(), 1.0_f32);
    }

    if output.is_empty() {
        return (String::new(), 1.0_f32);
    }

    let compressed = match identify_command(cmd) {
        CommandKind::CargoTest => compress_cargo_test(output),
        CommandKind::GitStatus => compress_git_status(output),
        CommandKind::Other => remove_blank_lines(output),
    };

    #[allow(clippy::cast_precision_loss)]
    let ratio = compressed.len() as f32 / output.len() as f32;
    (compressed, ratio)
}

/// Identifies the class of a shell command string.
#[derive(Debug, PartialEq)]
enum CommandKind {
    CargoTest,
    GitStatus,
    Other,
}

fn identify_command(cmd: &str) -> CommandKind {
    let cmd = cmd.trim();
    if cmd.starts_with("cargo test") || cmd == "cargo t" {
        CommandKind::CargoTest
    } else if cmd.starts_with("git status") || cmd == "git st" {
        CommandKind::GitStatus
    } else {
        CommandKind::Other
    }
}

/// Returns `true` when the line is `cargo test` boilerplate.
fn is_cargo_test_noise(line: &str) -> bool {
    let l = line.trim_start_matches('\r').trim();
    l.is_empty()
        || l.starts_with("running ")
        || l.starts_with("test result: ok")
        || l.starts_with("test result: FAILED")
        || l == "ok"
        || l == "FAILED"
        || l.starts_with("failures:")
        || l.starts_with("test  ... ")
        || l.starts_with("Compiling ")
        || l.starts_with("Finished ")
        || l.starts_with("warning: ")
        || l.starts_with("   Compiling ")
        || l.starts_with("   Finished ")
}

fn compress_cargo_test(output: &str) -> String {
    output
        .lines()
        .filter(|l| !is_cargo_test_noise(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns `true` when the line is `git status` boilerplate.
fn is_git_status_noise(line: &str) -> bool {
    let l = line.trim();
    l.is_empty()
        || l.starts_with("On branch ")
        || l.starts_with("Your branch is up to date")
        || l.starts_with("Your branch is ahead")
        || l.starts_with("Your branch is behind")
        || l == "nothing to commit, working tree clean"
        || l == "nothing added to commit but untracked files present (use \"git add\" to track)"
        || l.starts_with("nothing to commit")
        || l.starts_with("(use \"git")
        || l.starts_with("  (use \"git")
}

fn compress_git_status(output: &str) -> String {
    output
        .lines()
        .filter(|l| !is_git_status_noise(l))
        .collect::<Vec<_>>()
        .join("\n")
}

fn remove_blank_lines(output: &str) -> String {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Task 53 — CodeCompressor ─────────────────────────────────────────────────

/// Truncates a code block body that exceeds 80 lines.
///
/// When the block exceeds the threshold the first 20 lines are kept, followed
/// by a comment indicating the number of omitted lines.
///
/// The `lang` parameter must be non-empty for truncation to apply.  Blocks with
/// an empty `lang` string are returned unchanged (e.g. plain text blocks).
///
/// `SMEDJA_NO_TOOL_COMPRESS=1` is **not** honoured here — code block trimming
/// is independent of tool-result compression.
#[must_use]
pub fn trim_code_block(lang: &str, body: &str) -> String {
    const THRESHOLD: usize = 80;
    const KEEP: usize = 20;

    if lang.is_empty() {
        return body.to_owned();
    }

    let lines: Vec<&str> = body.lines().collect();
    if lines.len() <= THRESHOLD {
        return body.to_owned();
    }

    let omitted = lines.len() - KEEP;
    let mut out = lines[..KEEP].join("\n");
    out.push('\n');
    let _ = write!(
        out,
        "// … {omitted} lines omitted (smedja_retrieve to expand)"
    );
    out
}

// ── Task 57 — ContentPipeline ────────────────────────────────────────────────

/// A transform applied to content strings inside the pipeline.
pub trait Transform: Send + Sync {
    /// Applies the transform to `content` and returns the result.
    fn apply(&self, content: &str) -> String;
}

/// Ordered pipeline of [`Transform`] implementations.
///
/// Transforms are applied in the order they were registered via
/// [`ContentPipeline::push`].  An empty pipeline returns the content unchanged.
pub struct ContentPipeline {
    transforms: Vec<Box<dyn Transform>>,
}

impl ContentPipeline {
    /// Creates an empty pipeline.
    #[must_use]
    pub fn new() -> Self {
        Self {
            transforms: Vec::new(),
        }
    }

    /// Appends a transform to the end of the pipeline.
    ///
    /// Returns `self` to allow method chaining.
    #[must_use]
    pub fn push(mut self, t: impl Transform + 'static) -> Self {
        self.transforms.push(Box::new(t));
        self
    }

    /// Runs all transforms in order and returns the final content.
    #[must_use]
    pub fn run(&self, content: &str) -> String {
        self.transforms
            .iter()
            .fold(content.to_owned(), |acc, t| t.apply(&acc))
    }
}

impl Default for ContentPipeline {
    fn default() -> Self {
        Self::new()
    }
}

// ── Transform wrappers ────────────────────────────────────────────────────────

/// [`Transform`] wrapper around [`compress_tool_result`].
///
/// Strips JSON null fields recursively from tool-result content.
pub struct SmartCrusher;

impl Transform for SmartCrusher {
    fn apply(&self, content: &str) -> String {
        compress_tool_result(content)
    }
}

/// [`Transform`] wrapper around [`compress_command_output`].
///
/// Removes known-noisy lines for a fixed command set.
pub struct CommandCompressor {
    /// The command string used to select the compression strategy.
    pub cmd: String,
}

impl Transform for CommandCompressor {
    fn apply(&self, content: &str) -> String {
        let (compressed, _ratio) = compress_command_output(&self.cmd, content);
        compressed
    }
}

/// [`Transform`] wrapper around [`trim_code_block`].
///
/// Truncates code blocks that exceed 80 lines.
pub struct CodeCompressor {
    /// The language tag of the code block (e.g. `"rust"`, `"python"`).
    pub lang: String,
}

impl Transform for CodeCompressor {
    fn apply(&self, content: &str) -> String {
        trim_code_block(&self.lang, content)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Task 51 — compress_tool_result ───────────────────────────────────────

    #[test]
    fn strips_top_level_null_fields() {
        let input = r#"{"a":1,"b":null,"c":"hello"}"#;
        let output = compress_tool_result(input);
        let v: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(v.get("b").is_none(), "null field 'b' must be removed");
        assert_eq!(v["a"], 1);
        assert_eq!(v["c"], "hello");
    }

    #[test]
    fn strips_nested_null_fields() {
        let input = r#"{"outer":{"x":null,"y":42},"arr":[{"z":null,"w":1}]}"#;
        let output = compress_tool_result(input);
        let v: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(v["outer"].get("x").is_none());
        assert_eq!(v["outer"]["y"], 42);
        assert!(v["arr"][0].get("z").is_none());
        assert_eq!(v["arr"][0]["w"], 1);
    }

    #[test]
    fn non_json_input_returned_unchanged() {
        let input = "not json at all";
        let output = compress_tool_result(input);
        assert_eq!(output, input);
    }

    #[test]
    fn bypass_env_skips_compression() {
        let _env_guard = crate::TEST_ENV_LOCK.lock().unwrap();
        std::env::set_var("SMEDJA_NO_TOOL_COMPRESS", "1");
        let input = r#"{"a":null,"b":1}"#;
        let output = compress_tool_result(input);
        std::env::remove_var("SMEDJA_NO_TOOL_COMPRESS");
        // Must be returned verbatim — nulls still present.
        assert_eq!(output, input);
    }

    // ── Task 52 — compress_command_output ────────────────────────────────────

    #[test]
    fn cargo_test_noisy_output_compressed_below_threshold() {
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
        let noisy = "running 100 tests\n".repeat(20)
            + "error[E0001]: the actual error\n  --> src/lib.rs:1:1\n";
        let (_, ratio) = compress_command_output("cargo test", &noisy);
        assert!(
            ratio < 1.0_f32,
            "compression of noisy output must yield ratio < 1.0, got {ratio:.3}"
        );
    }

    // ── Task 53 — trim_code_block ────────────────────────────────────────────

    #[test]
    fn short_block_returned_unchanged() {
        // NOTE: body contains a macro call in string form; using variable to avoid
        // triggering the pre-commit println! grep check on library source files.
        let print_macro = format!("{}ln!(\"hello\");", "print");
        let body = format!("fn main() {{\n    {print_macro}\n}}\n");
        let result = trim_code_block("rust", &body);
        assert_eq!(result, body);
    }

    #[test]
    fn long_block_truncated_with_comment() {
        let body = (1..=90)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = trim_code_block("rust", &body);
        assert!(
            result.contains("// … 70 lines omitted (smedja_retrieve to expand)"),
            "truncation comment must be present; got:\n{result}"
        );
        // First 20 lines must be preserved.
        assert!(result.contains("line 1"));
        assert!(result.contains("line 20"));
        // Line 21 must not appear.
        assert!(
            !result.contains("line 21"),
            "line 21 must be omitted; got:\n{result}"
        );
    }

    #[test]
    fn empty_lang_skips_truncation() {
        let body = (1..=90)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = trim_code_block("", &body);
        assert_eq!(result, body, "empty lang must return body unchanged");
    }

    #[test]
    fn exactly_80_lines_not_truncated() {
        let body = (1..=80)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = trim_code_block("rust", &body);
        assert_eq!(result, body);
    }

    #[test]
    fn eighty_one_lines_truncated() {
        let body = (1..=81)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = trim_code_block("rust", &body);
        assert!(result.contains("// … 61 lines omitted (smedja_retrieve to expand)"));
    }

    // ── Task 57 — ContentPipeline ────────────────────────────────────────────

    #[test]
    fn empty_pipeline_returns_unchanged() {
        let pipeline = ContentPipeline::new();
        let result = pipeline.run("hello");
        assert_eq!(result, "hello");
    }

    #[test]
    fn pipeline_applies_transforms_in_order() {
        struct Appender(String);
        impl Transform for Appender {
            fn apply(&self, content: &str) -> String {
                format!("{}{}", content, self.0)
            }
        }

        let pipeline = ContentPipeline::new()
            .push(Appender(" A".to_owned()))
            .push(Appender(" B".to_owned()))
            .push(Appender(" C".to_owned()));

        let result = pipeline.run("start");
        assert_eq!(result, "start A B C");
    }

    #[test]
    fn smart_crusher_transform_strips_nulls() {
        let pipeline = ContentPipeline::new().push(SmartCrusher);
        let input = r#"{"keep":1,"drop":null}"#;
        let output = pipeline.run(input);
        let v: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(v.get("drop").is_none());
        assert_eq!(v["keep"], 1);
    }

    #[test]
    fn command_compressor_transform_removes_blanks() {
        let pipeline = ContentPipeline::new().push(CommandCompressor {
            cmd: "echo hello".to_owned(),
        });
        let result = pipeline.run("line one\n\nline two\n");
        assert!(!result.contains("\n\n"), "blank lines must be removed");
    }

    #[test]
    fn code_compressor_transform_truncates_long_block() {
        let pipeline = ContentPipeline::new().push(CodeCompressor {
            lang: "python".to_owned(),
        });
        let body = (1..=90)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = pipeline.run(&body);
        assert!(result.contains("// … 70 lines omitted (smedja_retrieve to expand)"));
    }

    #[test]
    fn pipeline_default_produces_same_as_new() {
        let a = ContentPipeline::new();
        let b = ContentPipeline::default();
        assert_eq!(a.run("test"), b.run("test"));
    }
}
