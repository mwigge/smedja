//! Context-assembly helpers for the turn orchestrator: per-tier retention
//! strata, model context-window sizing, and tool-result classification.

use smedja_assayer::Tier;
use smedja_memory::StrataConfig;
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
