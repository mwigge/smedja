//! Tier capability ranking and rotation-compatibility rules.

use smedja_assayer::Tier;

/// Capability rank of a [`Tier`] for rotation-compatibility comparisons.
///
/// Higher means more capable (larger context window / higher quality):
/// `Fast < Local < Deep`.
fn tier_capability_rank(tier: Tier) -> u8 {
    match tier {
        Tier::Fast => 0,
        Tier::Local => 1,
        Tier::Deep => 2,
    }
}

/// Returns `true` when a turn routed to `routed` may rotate to a provider of
/// tier `candidate` for a failure of the given `kind`.
///
/// Rotation must never degrade the turn below the routed tier, so `candidate`
/// must be at least as capable as `routed` (`Fast ≤ Local ≤ Deep`). For a
/// `context_length_exceeded` failure the candidate must be **strictly** more
/// capable — an equal-window provider would hit the same limit.
#[must_use]
pub fn tier_compatible(routed: Tier, candidate: Tier, kind: &str) -> bool {
    let routed_rank = tier_capability_rank(routed);
    let candidate_rank = tier_capability_rank(candidate);
    if kind == "context_length_exceeded" {
        candidate_rank > routed_rank
    } else {
        candidate_rank >= routed_rank
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_route_does_not_rotate_down_to_fast() {
        assert!(
            !tier_compatible(Tier::Deep, Tier::Fast, "rate_limited"),
            "a deep-routed turn must not rotate down to fast"
        );
        assert!(
            tier_compatible(Tier::Deep, Tier::Deep, "rate_limited"),
            "deep is compatible with itself"
        );
        assert!(
            tier_compatible(Tier::Fast, Tier::Deep, "rate_limited"),
            "fast may rotate up to deep"
        );
        assert!(
            tier_compatible(Tier::Fast, Tier::Local, "rate_limited"),
            "fast may rotate up to local"
        );
    }

    #[test]
    fn context_length_kind_requires_more_capable_tier() {
        assert!(
            !tier_compatible(Tier::Deep, Tier::Deep, "context_length_exceeded"),
            "context-length must not rotate to an equal-window tier"
        );
        assert!(
            tier_compatible(Tier::Fast, Tier::Deep, "context_length_exceeded"),
            "context-length may rotate to a strictly-more-capable tier"
        );
        assert!(
            !tier_compatible(Tier::Local, Tier::Fast, "context_length_exceeded"),
            "context-length must not rotate down"
        );
        assert!(
            tier_compatible(Tier::Local, Tier::Deep, "context_length_exceeded"),
            "local may rotate up to deep on context-length"
        );
    }
}
