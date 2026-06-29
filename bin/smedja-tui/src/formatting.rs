//! Pure text-formatting utilities used by the TUI key handler and render path.

/// Character-wraps `text` (honouring embedded `'\n'`) to `width` columns and
/// returns the visual rows.  Uses unicode-width so multi-byte and wide chars
/// are handled correctly.  `width` is clamped to ≥1.
#[must_use]
pub(crate) fn wrap_input_rows(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for logical in text.split('\n') {
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for ch in logical.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + cw > width && !cur.is_empty() {
                rows.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            cur.push(ch);
            cur_w += cw;
        }
        rows.push(cur);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

/// Returns the byte position of the previous char boundary before `pos`.
#[must_use]
pub(crate) fn prev_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos;
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p.saturating_sub(s[..p].chars().next_back().map_or(0, char::len_utf8))
}

/// Returns the byte position one char past `pos`.
#[must_use]
pub(crate) fn next_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    if p < s.len() {
        p + s[p..].chars().next().map_or(0, char::len_utf8)
    } else {
        p
    }
}

/// Extracts the content of the first ` ```{lang} … ``` ` fenced code block in `text`.
#[must_use]
pub(crate) fn extract_code_block<'a>(text: &'a str, lang: &str) -> Option<&'a str> {
    let fence_open = format!("```{lang}");
    let start = text.find(fence_open.as_str())?;
    let after_open = start + fence_open.len();
    let newline = text[after_open..].find('\n')?;
    let content_start = after_open + newline + 1;
    let end = text[content_start..].find("```")?;
    Some(text[content_start..content_start + end].trim())
}

/// Searches `history` backwards for the most recent entry containing `query`.
///
/// Returns `(index, matched_text)` on success.  An empty query always returns `None`.
#[must_use]
pub(crate) fn history_search<'a>(history: &'a [String], query: &str) -> Option<(usize, &'a str)> {
    if query.is_empty() {
        return None;
    }
    history
        .iter()
        .enumerate()
        .rev()
        .find(|(_, s)| s.contains(query))
        .map(|(i, s)| (i, s.as_str()))
}

/// Maps a turn-error message to `(short_label, hint)` for user-facing display.
#[must_use]
/// Formats a stream error for display, prefixing the runner name when known.
///
/// Output: `[runner · LABEL] message` or `[LABEL] message` when runner is empty.
pub(crate) fn format_turn_error(runner: &str, label: &str, message: &str) -> String {
    if runner.is_empty() {
        format!("[{label}] {message}")
    } else {
        format!("[{runner} \u{00b7} {label}] {message}")
    }
}

pub(crate) fn classify_turn_error(msg: &str) -> (&'static str, &'static str) {
    let lower = msg.to_lowercase();
    if lower.contains("rate limit") || lower.contains("rate_limit") {
        (
            "RATE LIMITED",
            "Use \u{2191} to recall your last message, then Enter to retry",
        )
    } else if lower.contains("api key")
        || lower.contains("auth")
        || lower.contains("401")
        || lower.contains("403")
    {
        (
            "AUTH ERROR",
            "Check ANTHROPIC_API_KEY or provider credentials",
        )
    } else if lower.contains("quota") || lower.contains("429") {
        ("QUOTA EXCEEDED", "Daily quota reached; check smj cost")
    } else if lower.contains("timeout") || lower.contains("timed out") {
        (
            "TIMEOUT",
            "Turn hit the wall-clock cap (default 900s; raise with SMEDJA_TURN_TIMEOUT_S)",
        )
    } else if lower.contains("network") || lower.contains("connection") || lower.contains("connect")
    {
        (
            "NETWORK ERROR",
            "Check network connectivity and provider endpoint",
        )
    } else if lower.contains("overload") {
        (
            "OVERLOADED",
            "Provider overloaded \u{2014} use \u{2191} and retry in a moment",
        )
    } else if lower.contains("context length") || lower.contains("maximum context") {
        (
            "CONTEXT FULL",
            "Context window full \u{2014} use /rollback to trim history",
        )
    } else {
        (
            "ERROR",
            "Check /obs for details or run smj session rollback",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_single_ascii_line_fits_width() {
        let rows = wrap_input_rows("hello", 10);
        assert_eq!(rows, vec!["hello"]);
    }

    #[test]
    fn wrap_long_line_splits_at_width() {
        let rows = wrap_input_rows("abcdefghij", 4);
        assert_eq!(rows, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_newlines_produce_separate_rows() {
        let rows = wrap_input_rows("abc\ndef", 10);
        assert_eq!(rows, vec!["abc", "def"]);
    }

    #[test]
    fn wrap_empty_input_returns_one_empty_row() {
        let rows = wrap_input_rows("", 10);
        assert_eq!(rows, vec![""]);
    }

    #[test]
    fn prev_char_boundary_moves_back_one_char() {
        let s = "hello";
        assert_eq!(prev_char_boundary(s, 3), 2);
    }

    #[test]
    fn next_char_boundary_moves_forward_one_char() {
        let s = "hello";
        assert_eq!(next_char_boundary(s, 2), 3);
    }

    #[test]
    fn extract_code_block_finds_fenced_content() {
        let text = "before\n```rust\nfn foo() {}\n```\nafter";
        assert_eq!(extract_code_block(text, "rust"), Some("fn foo() {}"));
    }

    #[test]
    fn extract_code_block_returns_none_when_missing() {
        assert!(extract_code_block("no code here", "rust").is_none());
    }

    #[test]
    fn history_search_returns_most_recent_match() {
        let history: Vec<String> = vec!["ls".into(), "grep foo".into(), "ls -la".into()];
        let (idx, text) = history_search(&history, "ls").unwrap();
        assert_eq!(idx, 2);
        assert_eq!(text, "ls -la");
    }

    #[test]
    fn history_search_empty_query_returns_none() {
        let history: Vec<String> = vec!["foo".into()];
        assert!(history_search(&history, "").is_none());
    }

    #[test]
    fn format_turn_error_includes_runner_and_label() {
        let s = format_turn_error("anthropic", "RATE LIMITED", "rate limit exceeded");
        assert!(s.contains("anthropic"), "{s}");
        assert!(s.contains("RATE LIMITED"), "{s}");
        assert!(s.contains("rate limit exceeded"), "{s}");
    }

    #[test]
    fn format_turn_error_falls_back_to_label_when_runner_is_unknown() {
        let s = format_turn_error("", "ERROR", "boom");
        assert!(s.contains("ERROR"), "{s}");
        assert!(s.contains("boom"), "{s}");
    }

    #[test]
    fn classify_turn_error_rate_limit() {
        let (label, _) = classify_turn_error("rate limit exceeded");
        assert_eq!(label, "RATE LIMITED");
    }

    #[test]
    fn classify_turn_error_auth() {
        let (label, _) = classify_turn_error("401 Unauthorized");
        assert_eq!(label, "AUTH ERROR");
    }

    #[test]
    fn classify_turn_error_generic() {
        let (label, _) = classify_turn_error("something unexpected");
        assert_eq!(label, "ERROR");
    }
}
