//! Context-pressure and summarisation helpers for the turn orchestrator.
//!
//! These functions decide when a turn's accumulated context crosses the
//! auto-compaction threshold and build the prompt used to summarise the
//! conversation so far. They are side-effect free and unit-tested in isolation.

/// Returns the auto-compact threshold from `val` (an optional env value string), defaulting to
/// 0.85. Values below 0.5 are clamped to 0.5 to prevent spurious compaction.
pub(crate) fn compact_threshold_from_env(val: Option<&str>) -> f64 {
    val.and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.85)
        .max(0.5)
}

/// Returns `true` when context fill exceeds the auto-summarisation threshold.
pub(crate) fn context_pressure_exceeds_threshold(
    input_tokens: u32,
    context_window: usize,
    threshold: f64,
) -> bool {
    if context_window == 0 {
        return false;
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = f64::from(input_tokens) / context_window as f64;
    ratio >= threshold
}

/// Builds the prompt sent to the LLM to produce a conversation summary.
///
/// At most 20 turns are included; older turns are dropped from the head.
pub(crate) fn build_summariser_prompt(history: &[(String, String)]) -> String {
    const MAX_TURNS: usize = 20;
    let turns: Vec<_> = history.iter().rev().take(MAX_TURNS).collect();
    let turns_text: String = turns
        .into_iter()
        .rev()
        .map(|(role, content)| format!("{role}: {content}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Produce a structured summary of the conversation so far. \
Format it as three clearly labelled sections using bullet points:\n\
- **Decisions**: key choices made and their rationale\n\
- **Changed files**: files created, edited, or deleted (with brief reason)\n\
- **Open questions**: unresolved issues or follow-up items\n\
Omit sections that have no content. Keep total length under 400 words.\n\n\
{turns_text}"
    )
}

#[cfg(test)]
mod tests {
    // --- build_summariser_prompt tests ---

    #[test]
    fn build_summariser_prompt_includes_history() {
        let history = vec![
            ("user".to_owned(), "fix the auth bug".to_owned()),
            (
                "assistant".to_owned(),
                "I found the issue in auth.rs".to_owned(),
            ),
        ];
        let prompt = super::build_summariser_prompt(&history);
        assert!(prompt.contains("fix the auth bug"));
        assert!(prompt.contains("I found the issue in auth.rs"));
    }

    #[test]
    fn build_summariser_prompt_has_instruction() {
        let prompt = super::build_summariser_prompt(&[]);
        assert!(prompt.contains("summarise") || prompt.contains("summary"));
    }

    #[test]
    fn build_summariser_prompt_caps_turns() {
        let history: Vec<(String, String)> = (0..30)
            .map(|i| ("user".to_owned(), format!("turn {i}")))
            .collect();
        let prompt = super::build_summariser_prompt(&history);
        // Should not include all 30 turns verbatim — cap enforced
        let turn_count = prompt.matches("turn ").count();
        assert!(turn_count <= 20, "too many turns: {turn_count}");
    }

    // --- context_pressure_exceeds_threshold tests ---

    #[test]
    fn pressure_below_threshold_is_not_exceeded() {
        assert!(!super::context_pressure_exceeds_threshold(
            79_999, 100_000, 0.85
        ));
    }

    #[test]
    fn pressure_at_threshold_is_exceeded() {
        assert!(super::context_pressure_exceeds_threshold(
            85_000, 100_000, 0.85
        ));
    }

    #[test]
    fn pressure_with_zero_window_is_never_exceeded() {
        assert!(!super::context_pressure_exceeds_threshold(1_000, 0, 0.85));
    }

    #[test]
    fn pressure_with_custom_threshold_respects_it() {
        assert!(super::context_pressure_exceeds_threshold(
            75_000, 100_000, 0.70
        ));
        assert!(!super::context_pressure_exceeds_threshold(
            74_999, 100_000, 0.75
        ));
    }

    #[test]
    fn compact_threshold_clamps_below_half() {
        // Values below 0.5 are clamped to 0.5 — safety guard.
        assert!(super::compact_threshold_from_env(Some("0.3")) >= 0.5);
    }

    #[test]
    fn compact_threshold_default_is_eighty_five_percent() {
        assert!((super::compact_threshold_from_env(None) - 0.85).abs() < f64::EPSILON);
    }

    #[test]
    fn compact_threshold_reads_env_value() {
        assert!((super::compact_threshold_from_env(Some("0.90")) - 0.90).abs() < f64::EPSILON);
    }
}
