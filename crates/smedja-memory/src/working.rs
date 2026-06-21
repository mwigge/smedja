//! Verbosity steering for the smedja context pipeline.

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
