//! Verbosity steering for the smedja context pipeline.
//!
//! # Context management pipeline
//!
//! Smedja applies context pressure through four cooperating mechanisms, in
//! approximately this order of precedence:
//!
//! 1. **Hot/warm strata windowing** (`smedja-memory/src/memory.rs`).
//!    Message history is split into a hot window (last 5 exchanges) and a warm
//!    window (last 30).  Only messages within the active window are forwarded to
//!    the provider; older messages are silently dropped.
//!
//! 2. **Verbosity steering** (this module — `working.rs`).
//!    When the assembled prompt exceeds 60 % of the provider's context window
//!    a conciseness directive is appended via [`inject_conciseness`].  The
//!    directive can be suppressed by setting `SMEDJA_NO_VERBOSITY_STEER=1`.
//!
//! 3. **Crush / compression** (`smedja-adapter/src/crush.rs`).
//!    Before messages reach the provider adapter, the crusher strips null tool
//!    results, compresses repeated shell commands, and truncates code blocks to
//!    80 lines.  This runs on every turn and is unaffected by fill percentage.
//!
//! 4. **Session compaction** (`session.compact` RPC in `smdjad/src/main.rs`).
//!    When the operator decides the context is too large, `session.compact`
//!    summarises the conversation history into 3–5 bullets and replaces the
//!    accumulated messages with the summary.  This is a destructive, one-way
//!    operation and must be triggered explicitly.

/// Appends a conciseness directive to `prompt` when the context window is more
/// than 60% full.
///
/// The directive is suppressed when the `SMEDJA_NO_VERBOSITY_STEER` environment
/// variable is set to `"1"`.
///
/// # Arguments
///
/// * `prompt` — the assembled prompt string.
/// * `used` — number of tokens currently used in the context window.
/// * `window` — total context window size in tokens.
#[must_use]
pub fn inject_conciseness(prompt: &str, used: usize, window: usize) -> String {
    if std::env::var("SMEDJA_NO_VERBOSITY_STEER").as_deref() == Ok("1") {
        return prompt.to_owned();
    }
    if window == 0 {
        return prompt.to_owned();
    }
    #[allow(clippy::cast_precision_loss)]
    // token counts are at most tens of thousands; f64 mantissa is sufficient
    let fill = used as f64 / window as f64;
    if fill > 0.60 {
        format!("{prompt}\n\nBe concise. Prefer short answers over long ones.")
    } else {
        prompt.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear_env() {
        // Remove the bypass env var if set from a previous test.
        // SAFETY: test-only mutation; no concurrent threads read this var in tests.
        unsafe { std::env::remove_var("SMEDJA_NO_VERBOSITY_STEER") };
    }

    #[test]
    fn below_threshold_unchanged() {
        clear_env();
        // 59% fill → no directive
        let out = inject_conciseness("hello", 59, 100);
        assert_eq!(out, "hello");
    }

    #[test]
    fn above_threshold_appends_directive() {
        clear_env();
        // 61% fill → directive appended
        let out = inject_conciseness("hello", 61, 100);
        assert!(
            out.ends_with("\n\nBe concise. Prefer short answers over long ones."),
            "expected directive appended, got: {out:?}"
        );
        assert!(out.starts_with("hello"));
    }

    #[test]
    fn env_bypass_returns_unchanged() {
        // SAFETY: test-only mutation; no concurrent threads read this var in tests.
        unsafe { std::env::set_var("SMEDJA_NO_VERBOSITY_STEER", "1") };
        let out = inject_conciseness("hello", 99, 100);
        assert_eq!(out, "hello", "env bypass must suppress directive");
        // SAFETY: test-only mutation; no concurrent threads read this var in tests.
        unsafe { std::env::remove_var("SMEDJA_NO_VERBOSITY_STEER") };
    }

    #[test]
    fn exactly_sixty_percent_unchanged() {
        clear_env();
        // exactly 60% → not > 0.60 → no directive
        let out = inject_conciseness("test", 60, 100);
        assert_eq!(out, "test");
    }
}
