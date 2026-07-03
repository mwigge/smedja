//! The `(Runner, Tier)` provider map and its rotation-ring accessors.

use std::collections::HashMap;

use smedja_adapter::Provider;
use smedja_assayer::{Runner, Tier};

use crate::provider_pool::local_control::LocalControl;
use crate::provider_pool::tier::tier_compatible;

/// A single pool entry: the provider plus the strings needed for logging and
/// session-store keying.
pub struct ProviderEntry {
    pub provider: Box<dyn Provider>,
    /// The routing runner this entry serves — drives the session-resume store key.
    pub runner: Runner,
    /// The routing tier this entry serves.
    pub tier: Tier,
    /// Short identifier used in logs and the session-resume store key.
    pub runner_name: &'static str,
    /// Default model name when no session override is set. Resolved at pool-build
    /// time from a built-in default or a `SMEDJA_MODEL_<RUNNER>_<TIER>` env override
    /// (see [`crate::provider_pool::model_default`]).
    pub default_model: String,
}

/// Map from `(Runner, Tier)` to a concrete provider instance.
///
/// Built once at daemon start-up; shared across all concurrent turns via
/// `Arc<ProviderPool>`.
pub struct ProviderPool {
    pub(crate) entries: HashMap<(Runner, Tier), ProviderEntry>,
    /// Keys in stable insertion/priority order — the same order
    /// [`crate::provider_pool::build_provider_pool`] probes providers, which
    /// determines the default and the rotation-ring priority. A `HashMap` does
    /// not preserve insertion order, so the ordering is tracked explicitly here.
    pub(crate) order: Vec<(Runner, Tier)>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_adapter::{GpuSnapshot, LocalModel};

    struct NullProvider;
    impl smedja_adapter::Provider for NullProvider {
        fn stream_chat(
            &self,
            _messages: &[smedja_adapter::Message],
            _opts: &smedja_adapter::CallOptions,
        ) -> smedja_adapter::DeltaStream {
            Box::pin(futures_util::stream::empty())
        }
    }

    fn pool_with(entries: Vec<((Runner, Tier), &'static str, &'static str)>) -> ProviderPool {
        let mut map = HashMap::new();
        let mut order = Vec::new();
        let mut default = None;
        for (key, runner_name, default_model) in entries {
            if default.is_none() {
                default = Some(key);
            }
            if map
                .insert(
                    key,
                    ProviderEntry {
                        provider: Box::new(NullProvider),
                        runner: key.0,
                        tier: key.1,
                        runner_name,
                        default_model: default_model.to_owned(),
                    },
                )
                .is_none()
            {
                order.push(key);
            }
        }
        ProviderPool {
            entries: map,
            order,
            default,
            local: None,
        }
    }

    #[test]
    fn local_control_exposes_inventory_and_mutable_active_model() {
        let control = LocalControl::new(
            "http://127.0.0.1:9090".to_owned(),
            "http://127.0.0.1:9090".to_owned(),
            vec![
                LocalModel {
                    id: "qwen3-14b".to_owned(),
                    est_vram_mb: Some(9000),
                },
                LocalModel {
                    id: "llama3-8b".to_owned(),
                    est_vram_mb: None,
                },
            ],
            GpuSnapshot::none(),
            Some("qwen3-14b".to_owned()),
        );
        let pool = ProviderPool {
            entries: HashMap::new(),
            order: Vec::new(),
            default: None,
            local: Some(control),
        };

        let local = pool.local_control().expect("local control present");
        let ids: Vec<&str> = local.inventory.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["qwen3-14b", "llama3-8b"],
            "pool entry must expose the full local inventory"
        );
        assert_eq!(local.active_model_id().as_deref(), Some("qwen3-14b"));

        // The active model must be mutable in place without rebuilding the pool.
        let previous = local.set_active_model_id("llama3-8b");
        assert_eq!(previous.as_deref(), Some("qwen3-14b"));
        assert_eq!(local.active_model_id().as_deref(), Some("llama3-8b"));
    }

    #[test]
    fn get_returns_exact_match() {
        let pool = pool_with(vec![
            (
                (Runner::Claude, Tier::Fast),
                "claude-cli",
                "claude-haiku-4-5-20251001",
            ),
            ((Runner::Local, Tier::Local), "local", "local"),
        ]);
        let entry = pool.get(Runner::Local, Tier::Local).unwrap();
        assert_eq!(entry.runner_name, "local");
    }

    #[test]
    fn get_falls_back_to_default() {
        let pool = pool_with(vec![(
            (Runner::Claude, Tier::Fast),
            "claude-cli",
            "claude-haiku-4-5-20251001",
        )]);
        // Codex is not in the pool; should fall back to default (claude-cli).
        let entry = pool.get(Runner::Codex, Tier::Fast).unwrap();
        assert_eq!(entry.runner_name, "claude-cli");
    }

    #[test]
    fn get_returns_none_on_empty_pool() {
        let pool = ProviderPool {
            entries: HashMap::new(),
            order: Vec::new(),
            default: None,
            local: None,
        };
        assert!(pool.get(Runner::Claude, Tier::Fast).is_none());
    }

    #[test]
    fn empty_pool_reports_is_empty() {
        let pool = ProviderPool {
            entries: std::collections::HashMap::new(),
            order: Vec::new(),
            default: None,
            local: None,
        };
        assert!(
            pool.is_empty(),
            "a pool with no providers must report empty"
        );
        assert_eq!(pool.default_runner_name(), "unknown");
    }

    #[test]
    fn default_runner_name_returns_first_inserted() {
        let pool = pool_with(vec![
            (
                (Runner::Claude, Tier::Fast),
                "claude-cli",
                "claude-haiku-4-5-20251001",
            ),
            ((Runner::Local, Tier::Local), "local", "local"),
        ]);
        assert_eq!(pool.default_runner_name(), "claude-cli");
    }

    #[test]
    fn available_runners_lists_unique_names() {
        let pool = pool_with(vec![
            (
                (Runner::Claude, Tier::Fast),
                "claude-cli",
                "claude-haiku-4-5-20251001",
            ),
            (
                (Runner::Claude, Tier::Deep),
                "claude-cli",
                "claude-sonnet-4-6",
            ),
            ((Runner::Local, Tier::Local), "local", "local"),
        ]);
        let mut runners = pool.available_runners();
        runners.sort_unstable();
        assert_eq!(runners, vec!["claude-cli", "local"]);
    }

    #[test]
    fn pool_with_only_local_provider_returns_local() {
        let pool = pool_with(vec![((Runner::Local, Tier::Local), "local", "local")]);
        // All routes fall back to local.
        let entry = pool.get(Runner::Claude, Tier::Deep).unwrap();
        assert_eq!(entry.runner_name, "local");
    }

    #[test]
    fn list_all_entries_returns_runner_tier_model_triples() {
        let pool = pool_with(vec![
            (
                (Runner::Claude, Tier::Fast),
                "claude-cli",
                "claude-haiku-4-5-20251001",
            ),
            (
                (Runner::Claude, Tier::Deep),
                "claude-cli",
                "claude-sonnet-4-6",
            ),
            ((Runner::Local, Tier::Local), "local", "qwen3-14b"),
        ]);
        let entries = pool.list_all_entries();
        assert_eq!(entries.len(), 3);
        let tiers: Vec<&str> = entries.iter().map(|&(_, t, _)| t).collect();
        assert!(
            tiers.contains(&"fast"),
            "fast tier must appear in list_all_entries"
        );
        assert!(
            tiers.contains(&"deep"),
            "deep tier must appear in list_all_entries"
        );
        assert!(
            tiers.contains(&"local"),
            "local tier must appear in list_all_entries"
        );
    }

    #[test]
    fn eligible_ring_orders_routed_first_then_compatible_dedup() {
        // Insertion/priority order: claude-fast (default), claude-deep, local.
        let pool = pool_with(vec![
            (
                (Runner::Claude, Tier::Fast),
                "claude-cli",
                "claude-haiku-4-5-20251001",
            ),
            (
                (Runner::Claude, Tier::Deep),
                "claude-cli",
                "claude-sonnet-4-6",
            ),
            ((Runner::Local, Tier::Local), "local", "local"),
        ]);

        // Route to fast: ring starts with the routed fast entry, then the more
        // capable local and deep entries in priority order, ending with the
        // default (already yielded → not duplicated).
        let ring = pool.eligible_ring(Runner::Claude, Tier::Fast);
        let names: Vec<&str> = ring.iter().map(|e| e.runner_name).collect();
        assert_eq!(
            names,
            vec!["claude-cli", "claude-cli", "local"],
            "ring must be routed-first then compatible entries in priority order"
        );

        // Every (Runner, Tier) appears at most once: ring length never exceeds
        // the number of distinct entries.
        assert_eq!(ring.len(), 3, "ring must de-duplicate by (Runner, Tier)");
    }

    #[test]
    fn eligible_ring_routes_deep_excludes_fast_entries() {
        let pool = pool_with(vec![
            (
                (Runner::Claude, Tier::Fast),
                "claude-cli",
                "claude-haiku-4-5-20251001",
            ),
            (
                (Runner::Claude, Tier::Deep),
                "claude-cli",
                "claude-sonnet-4-6",
            ),
        ]);
        let ring = pool.eligible_ring(Runner::Claude, Tier::Deep);
        // Routed deep entry only; the fast entry is less capable and excluded.
        // The default (claude-fast) is incompatible so it is not appended.
        assert_eq!(ring.len(), 1, "deep route must exclude the fast entry");
    }
}
