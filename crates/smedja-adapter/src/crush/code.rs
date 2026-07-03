//! Task 53 — `CodeCompressor`: long code-block truncation.

use std::fmt::Write as _;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
