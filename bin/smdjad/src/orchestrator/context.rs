//! Context-assembly helpers for the turn orchestrator: per-tier retention
//! strata, model context-window sizing, tool-result classification, and
//! per-runner cache-strategy selection.

use smedja_adapter::CacheStrategy;
use smedja_assayer::Tier;
use smedja_memory::{CacheHint, Drift, StrataConfig};
use smedja_types::ToolOutcome;

/// Maps a routed runner tier to its retention strata and warm-stratum token
/// budget. `fast` keeps a shallow warm window and small budget; `deep` keeps the
/// full warm window and a large budget; `local` sits between. The stable prefix
/// and hot turns are always included verbatim regardless of budget.
pub(crate) fn strata_for_tier(tier: Tier) -> (StrataConfig, usize) {
    match tier {
        Tier::Fast => (StrataConfig::fast(), 4_000),
        Tier::Local => (StrataConfig::local(), 8_000),
        Tier::Deep => (StrataConfig::deep(), 32_000),
    }
}

/// Maps a routed runner tier to the number of cold-stratum results to recall.
///
/// `fast` favours latency with a single hit; `deep` favours recall with up to
/// five. The orchestrator caps the assembled cold block by the tier token
/// budget regardless of this count.
pub(crate) fn cold_k_for_tier(tier: Tier) -> usize {
    match tier {
        Tier::Fast => 1,
        Tier::Local => 3,
        Tier::Deep => 5,
    }
}

/// Returns the approximate context-window size (in tokens) for a model.
///
/// Used to scale verbosity steering — the conciseness directive is appended once
/// the assembled prompt exceeds 60% of this window. Unknown models fall back to a
/// conservative 128k window.
pub(crate) fn model_context_window(model: &str) -> usize {
    if model.to_lowercase().contains("claude") {
        200_000
    } else {
        // gpt-4o / o1 / o3 and unknown models share the conservative default.
        128_000
    }
}

/// Selects the per-runner `(stable_prefix_len, cache_strategy)` from an aligner
/// [`CacheHint`].
///
/// The breakpoint from the aligner is the safe stable-prefix length for every
/// cache-capable provider. When the aligner reports [`Drift::Mutated`] with no
/// stable remainder (a zero-length breakpoint), no cache hint is realised this
/// turn: `stable_prefix_len` falls back to `None` and the strategy is
/// [`CacheStrategy::None`], so the request behaves like an uncached one.
///
/// - `"anthropic"` → [`CacheStrategy::AnthropicEphemeral`] (driven by
///   `stable_prefix_len`, identical to the shipped behaviour).
/// - `"openai"` → [`CacheStrategy::OpenAiAutomatic`] with the supplied key.
/// - `"gemini"` → [`CacheStrategy::GeminiContext`] with the supplied handle.
/// - any other runner → no cache hint.
pub(crate) fn cache_options_for_runner(
    runner_name: &str,
    hint: CacheHint,
    openai_cache_key: Option<String>,
    gemini_cached_content: Option<String>,
) -> (Option<usize>, CacheStrategy) {
    // No stable prefix remains (e.g. a mutated first message): send no hint.
    if hint.drift == Drift::Mutated && hint.breakpoint == 0 {
        return (None, CacheStrategy::None);
    }

    let prefix_len = Some(hint.breakpoint);
    match runner_name {
        "anthropic" => (prefix_len, CacheStrategy::AnthropicEphemeral),
        "openai" => (
            prefix_len,
            CacheStrategy::OpenAiAutomatic {
                cache_key: openai_cache_key,
            },
        ),
        "gemini" => (
            prefix_len,
            CacheStrategy::GeminiContext {
                cached_content: gemini_cached_content,
            },
        ),
        _ => (None, CacheStrategy::None),
    }
}

/// Classifies an `execute_tool` result string into a [`ToolOutcome`].
///
/// The tool layer signals failures through textual prefixes rather than a typed
/// error, so this bridges that convention onto the typed outcome: an
/// `"error:"`-prefixed result that mentions a timeout becomes
/// [`ToolOutcome::Timeout`], other `"error:"` results become
/// [`ToolOutcome::Failure`], `"permission denied"` becomes
/// [`ToolOutcome::ApprovalDenied`], and everything else is
/// [`ToolOutcome::Success`].  The text fed back to the agent is unchanged.
pub(crate) fn classify_tool_outcome(result: &str) -> ToolOutcome {
    if result.starts_with("permission denied") {
        ToolOutcome::ApprovalDenied(result.to_owned())
    } else if result.starts_with("error:") {
        if result.contains("timed out") || result.contains("timeout") {
            ToolOutcome::Timeout
        } else {
            ToolOutcome::Failure(result.to_owned())
        }
    } else {
        ToolOutcome::Success(result.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::cache_options_for_runner;
    use smedja_adapter::CacheStrategy;
    use smedja_memory::{CacheHint, Drift};

    fn grown(breakpoint: usize) -> CacheHint {
        CacheHint {
            breakpoint,
            drift: Drift::Grown,
        }
    }

    #[test]
    fn anthropic_uses_breakpoint_and_ephemeral_strategy() {
        let (len, strategy) = cache_options_for_runner("anthropic", grown(3), None, None);
        assert_eq!(len, Some(3));
        assert_eq!(strategy, CacheStrategy::AnthropicEphemeral);
    }

    #[test]
    fn openai_uses_automatic_strategy_with_key() {
        let (len, strategy) =
            cache_options_for_runner("openai", grown(2), Some("sess-1".to_owned()), None);
        assert_eq!(len, Some(2));
        assert_eq!(
            strategy,
            CacheStrategy::OpenAiAutomatic {
                cache_key: Some("sess-1".to_owned())
            }
        );
    }

    #[test]
    fn gemini_uses_context_strategy_with_handle() {
        let (len, strategy) = cache_options_for_runner(
            "gemini",
            grown(4),
            None,
            Some("cachedContents/x".to_owned()),
        );
        assert_eq!(len, Some(4));
        assert_eq!(
            strategy,
            CacheStrategy::GeminiContext {
                cached_content: Some("cachedContents/x".to_owned())
            }
        );
    }

    #[test]
    fn unknown_runner_gets_no_hint() {
        let (len, strategy) = cache_options_for_runner("local", grown(3), None, None);
        assert_eq!(len, None);
        assert_eq!(strategy, CacheStrategy::None);
    }

    #[test]
    fn mutated_with_no_stable_remainder_sends_no_hint() {
        let mutated = CacheHint {
            breakpoint: 0,
            drift: Drift::Mutated,
        };
        let (len, strategy) = cache_options_for_runner("anthropic", mutated, None, None);
        assert_eq!(len, None, "no stable prefix when nothing survived mutation");
        assert_eq!(strategy, CacheStrategy::None);
    }

    #[test]
    fn anthropic_stable_prefix_len_matches_sealed_prefix_on_first_turn() {
        use smedja_memory::{CacheAligner, WorkingMemory};
        // A freshly sealed WorkingMemory observed by a new aligner yields a
        // breakpoint equal to stable_prefix(); the Anthropic runner must receive
        // exactly that as stable_prefix_len (parity with the shipped behaviour).
        let mut mem = WorkingMemory::new(4096);
        mem.push(smedja_adapter::types::Message::system("sys"));
        mem.push(smedja_adapter::types::Message::user("first"));
        mem.seal_prefix();
        let hint = CacheAligner::new().align(&mem);
        let (len, strategy) = cache_options_for_runner("anthropic", hint, None, None);
        assert_eq!(len, Some(mem.stable_prefix()));
        assert_eq!(strategy, CacheStrategy::AnthropicEphemeral);
    }

    #[test]
    fn mutated_with_surviving_prefix_still_hints() {
        let mutated = CacheHint {
            breakpoint: 1,
            drift: Drift::Mutated,
        };
        let (len, strategy) = cache_options_for_runner("anthropic", mutated, None, None);
        assert_eq!(len, Some(1));
        assert_eq!(strategy, CacheStrategy::AnthropicEphemeral);
    }

    #[test]
    fn fast_tier_prompt_no_larger_than_deep_with_hot_present() {
        use smedja_adapter::types::Message;
        use smedja_assayer::Tier;
        use smedja_memory::WorkingMemory;

        let build = |tier: Tier| {
            let (strata, budget) = super::strata_for_tier(tier);
            let mut m = WorkingMemory::new(budget);
            m.set_strata(strata);
            m.push(Message::user("stable context")); // prefix
            m.seal_prefix();
            for i in 0..40 {
                m.push(Message::user(format!(
                    "turn {i} with enough content to cost a few tokens each"
                )));
            }
            m.build_prompt(budget)
        };

        let fast = build(Tier::Fast);
        let deep = build(Tier::Deep);

        // A shallower/cheaper tier must never assemble more messages than deep.
        assert!(
            fast.len() <= deep.len(),
            "fast prompt ({}) must be ≤ deep prompt ({})",
            fast.len(),
            deep.len()
        );
        // The most recent hot turn must be present in both regardless of tier.
        assert!(
            fast.iter().any(|m| m.content.contains("turn 39")),
            "fast must retain the latest hot turn"
        );
        assert!(
            deep.iter().any(|m| m.content.contains("turn 39")),
            "deep must retain the latest hot turn"
        );
    }

    #[test]
    fn model_context_window_known_and_default() {
        assert_eq!(super::model_context_window("claude-sonnet-4-6"), 200_000);
        assert_eq!(super::model_context_window("some-unknown-model"), 128_000);
    }
}
