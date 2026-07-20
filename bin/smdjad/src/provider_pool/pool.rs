//! The provider pool/registry: the (Runner, Tier) -> ProviderEntry map and
//! the rotation-ring / tier-compatibility logic over it.

use std::collections::HashMap;

use smedja_assayer::{Runner, Tier};

use super::types::{LocalControl, ProviderEntry};
use crate::price_table::PriceTable;
use tracing::warn;

/// Map from `(Runner, Tier)` to a concrete provider instance.
///
/// Built once at daemon start-up; shared across all concurrent turns via
/// `Arc<ProviderPool>`.
pub struct ProviderPool {
    pub(super) entries: HashMap<(Runner, Tier), ProviderEntry>,
    /// Keys in stable insertion/priority order — the same order
    /// [`build_provider_pool`] probes providers, which determines the default
    /// and the rotation-ring priority. A `HashMap` does not preserve insertion
    /// order, so the ordering is tracked explicitly here.
    pub(super) order: Vec<(Runner, Tier)>,
    /// The `(Runner, Tier)` used when the assayer selects a route that has no
    /// entry in the pool.
    pub default: Option<(Runner, Tier)>,
    /// Control-plane state for the `local` runner, present only when a healthy
    /// local endpoint was detected at startup. `None` means the `local.*` RPCs
    /// report "local tooling unavailable".
    pub local: Option<LocalControl>,
}

impl ProviderPool {
    /// Returns the `local` runner control plane, or `None` when no healthy local
    /// endpoint was detected at startup.
    #[must_use]
    pub fn local_control(&self) -> Option<&LocalControl> {
        self.local.as_ref()
    }
}

impl ProviderPool {
    /// Builds a pool from an explicit, ordered list of entries.
    ///
    /// The first entry becomes the default. Used by orchestrator tests to inject
    /// mock providers in a known rotation order.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_entries_for_test(entries: Vec<((Runner, Tier), ProviderEntry)>) -> Self {
        let mut map = HashMap::new();
        let mut order = Vec::new();
        let mut default = None;
        for (key, entry) in entries {
            if default.is_none() {
                default = Some(key);
            }
            if map.insert(key, entry).is_none() {
                order.push(key);
            }
        }
        Self {
            entries: map,
            order,
            default,
            local: None,
        }
    }

    /// Returns the ordered, de-duplicated ring of pool entries eligible to serve
    /// a turn routed to `(runner, tier)`.
    ///
    /// The ring yields, in order: the exact routed entry first (if present),
    /// then other entries whose tier is compatible with the routed tier in the
    /// pool's stable priority order, and finally the pool default if not already
    /// yielded. Each `(Runner, Tier)` key appears at most once, so the ring is
    /// finite. Compatibility uses the rotation `kind` `"rate_limited"` semantics
    /// (any tier no less capable than the routed tier); callers that rotate on a
    /// context-length failure must additionally filter the ring with
    /// [`tier_compatible`].
    #[must_use]
    pub fn eligible_ring(&self, runner: Runner, tier: Tier) -> Vec<&ProviderEntry> {
        let mut seen = std::collections::HashSet::new();
        let mut ring: Vec<&ProviderEntry> = Vec::new();

        // 1. The exact routed entry first.
        if let Some(entry) = self.entries.get(&(runner, tier)) {
            if seen.insert((runner, tier)) {
                ring.push(entry);
            }
        }

        // 2. Other compatible-tier entries in stable priority order.
        for &(r, t) in &self.order {
            if seen.contains(&(r, t)) {
                continue;
            }
            if tier_compatible(tier, t, "rate_limited") {
                if let Some(entry) = self.entries.get(&(r, t)) {
                    seen.insert((r, t));
                    ring.push(entry);
                }
            }
        }

        // 3. The pool default last, if not already present and still compatible.
        //    Compatibility is enforced even for the default so a turn never
        //    rotates below its routed tier.
        if let Some(key @ (_, default_tier)) = self.default {
            if !seen.contains(&key) && tier_compatible(tier, default_tier, "rate_limited") {
                if let Some(entry) = self.entries.get(&key) {
                    seen.insert(key);
                    ring.push(entry);
                }
            }
        }

        ring
    }

    /// Returns the entry for `(runner, tier)`, falling back to the pool's
    /// default when that key is absent.  Returns `None` only when the pool is
    /// completely empty.
    #[must_use]
    pub fn get(&self, runner: Runner, tier: Tier) -> Option<&ProviderEntry> {
        self.entries
            .get(&(runner, tier))
            .or_else(|| self.default.as_ref().and_then(|d| self.entries.get(d)))
    }

    /// Short runner-name string for the default provider (e.g. `"claude-cli"`).
    ///
    /// Used to populate the `session.create` RPC response before any turn runs.
    #[must_use]
    pub fn default_runner_name(&self) -> &'static str {
        self.default
            .as_ref()
            .and_then(|d| self.entries.get(d))
            .map_or("unknown", |e| e.runner_name)
    }

    /// Default model name for the pool's primary provider.
    #[must_use]
    pub fn default_model(&self) -> &str {
        self.default
            .as_ref()
            .and_then(|d| self.entries.get(d))
            .map_or("", |e| e.default_model.as_str())
    }

    /// Returns the default pool entry, or `None` when the pool is empty.
    #[must_use]
    pub fn get_default(&self) -> Option<&ProviderEntry> {
        self.default.as_ref().and_then(|d| self.entries.get(d))
    }

    /// Returns `true` when no provider is configured — every turn will fail and
    /// the daemon is in a loud degraded state.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the names of all runners currently available in the pool.
    #[must_use]
    pub fn available_runners(&self) -> Vec<&'static str> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for entry in self.entries.values() {
            if seen.insert(entry.runner_name) {
                out.push(entry.runner_name);
            }
        }
        out
    }

    /// Returns the list of `(runner_name, model)` pairs in this pool that have
    /// no price entry. De-duplicated and sorted for stable output.
    #[must_use]
    pub fn missing_price_models(&self, price_table: &PriceTable) -> Vec<(&str, &str)> {
        let mut missing: Vec<(&str, &str)> = self
            .entries
            .values()
            .filter_map(|entry| {
                let model = entry.default_model.as_str();
                if price_table.compute_cost(model, 1_000_000, 0).abs() < f64::EPSILON
                    && price_table.compute_cost(model, 0, 1_000_000).abs() < f64::EPSILON
                {
                    Some((entry.runner_name, model))
                } else {
                    None
                }
            })
            .collect();
        missing.sort();
        missing.dedup();
        missing
    }

    /// Logs a warning for every pool entry whose default model has no price
    /// entry, so cost metrics silently reporting `$0.00` are surfaced at startup.
    pub fn warn_missing_prices(&self, price_table: &PriceTable) {
        for (runner, model) in self.missing_price_models(price_table) {
            warn!(
                runner = runner,
                model = model,
                "no price entry for model — session cost and metrics will report $0.00 until prices.toml is updated"
            );
        }
    }

    /// Returns all pool entries as `(runner_name, tier, default_model)` triples.
    ///
    /// Sorted by runner name then tier so callers get a stable, human-readable order.
    #[must_use]
    pub fn list_all_entries(&self) -> Vec<(&'static str, &'static str, &str)> {
        let tier_str = |t: &Tier| match t {
            Tier::Fast => "fast",
            Tier::Deep => "deep",
            Tier::Local => "local",
        };
        let mut out: Vec<_> = self
            .entries
            .iter()
            .map(|((_, tier), entry)| {
                (
                    entry.runner_name,
                    tier_str(tier),
                    entry.default_model.as_str(),
                )
            })
            .collect();
        out.sort_by_key(|&(runner, tier, _)| (runner, tier));
        out
    }
}

/// Capability rank of a [`Tier`] for rotation-compatibility comparisons.
///
/// Higher means more capable (larger context window / higher quality):
/// `Local < Fast < Deep`. Delegates to [`Tier::capability_rank`] — the single
/// source of truth shared with the assayer so the two orderings never diverge.
fn tier_capability_rank(tier: Tier) -> u8 {
    tier.capability_rank()
}

/// Returns `true` when a turn routed to `routed` may rotate to a provider of
/// tier `candidate` for a failure of the given `kind`.
///
/// Rotation must never degrade the turn below the routed tier, so `candidate`
/// must be at least as capable as `routed` (`Local ≤ Fast ≤ Deep`). For a
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
    use super::{tier_capability_rank, tier_compatible};
    use smedja_assayer::Tier;

    const TIERS: [Tier; 3] = [Tier::Local, Tier::Fast, Tier::Deep];

    #[test]
    fn tier_capability_rank_matches_canonical_source() {
        // Derived from Tier::capability_rank — must agree tier-for-tier.
        for tier in TIERS {
            assert_eq!(tier_capability_rank(tier), tier.capability_rank());
        }
    }

    #[test]
    fn tier_capability_rank_agrees_with_assayer_descent_ladder() {
        // The assayer's descent ladder goes Deep → Fast → Local (strongest to
        // cheapest). The pool's rotation ranking must place them in the same
        // relative order for every pair, or failover could pick the wrong tier.
        for a in TIERS {
            for b in TIERS {
                let pool_ord = tier_capability_rank(a).cmp(&tier_capability_rank(b));
                let canonical_ord = a.capability_rank().cmp(&b.capability_rank());
                assert_eq!(pool_ord, canonical_ord, "disagree on {a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn rotation_never_degrades_fast_turn_to_local() {
        // Regression: with the old `Fast < Local` ranking, a Fast turn was
        // allowed to rotate down to a weaker Local provider. It must not.
        assert!(!tier_compatible(Tier::Fast, Tier::Local, "server_error"));
        // A Fast turn may still rotate up to Deep.
        assert!(tier_compatible(Tier::Fast, Tier::Deep, "server_error"));
        // Same tier is fine for a non-context failure.
        assert!(tier_compatible(Tier::Fast, Tier::Fast, "server_error"));
    }
}
