//! Runtime pool of available LLM providers, indexed by (Runner, Tier).

use std::collections::HashMap;
use std::sync::Mutex;

use smedja_adapter::{
    AnthropicProvider, BergetProvider, ClaudeCliProvider, CodexCliProvider, CopilotProvider,
    GpuSnapshot, LocalModel, LocalProvider, MinimaxProvider, OpenAiProvider, PoolsideProvider,
    Provider, SubprocessProvider,
};
use smedja_assayer::{Runner, Tier};
use tracing::{error, info, warn};

/// Control-plane state for the `local` runner: the swap-proxy endpoint, the full
/// model inventory, the cached GPU snapshot, and the active-model selection.
///
/// The active-model id lives behind a [`Mutex`] so `local.swap` can update it in
/// place — atomically, without rebuilding the pool's `stream_chat` provider —
/// while concurrent turns keep the model they started with.
pub struct LocalControl {
    /// OpenAI-compatible base endpoint (`SMEDJA_LOCAL_ENDPOINT`); re-queried for
    /// `/v1/models` after an install to verify the model is servable.
    pub endpoint: String,
    /// Swap-proxy endpoint (`SMEDJA_LOCAL_SWAP_ENDPOINT`) the hot-swap targets.
    pub swap_endpoint: String,
    /// Full `/v1/models` inventory captured at connect time.
    pub inventory: Vec<LocalModel>,
    /// Advisory GPU snapshot captured at startup; refreshed on demand by `local.gpu`.
    pub gpu: GpuSnapshot,
    /// The active local model id, mutated in place by a hot-swap.
    active_model_id: Mutex<Option<String>>,
}

impl LocalControl {
    /// Builds a control plane from its captured parts.
    #[must_use]
    pub fn new(
        endpoint: String,
        swap_endpoint: String,
        inventory: Vec<LocalModel>,
        gpu: GpuSnapshot,
        active_model_id: Option<String>,
    ) -> Self {
        Self {
            endpoint,
            swap_endpoint,
            inventory,
            gpu,
            active_model_id: Mutex::new(active_model_id),
        }
    }

    /// Returns the currently-active local model id, if any.
    ///
    /// # Panics
    ///
    /// Panics only if the active-model lock was poisoned by a prior panic while
    /// holding it — a non-recoverable invariant violation.
    #[must_use]
    pub fn active_model_id(&self) -> Option<String> {
        self.active_model_id
            .lock()
            .expect("local active-model lock poisoned")
            .clone()
    }

    /// Atomically sets the active local model id, returning the previous value.
    ///
    /// Called by `local.swap` after the swap proxy accepts the request; no
    /// provider object is rebuilt.
    ///
    /// # Panics
    ///
    /// Panics only if the active-model lock was poisoned by a prior panic while
    /// holding it — a non-recoverable invariant violation.
    pub fn set_active_model_id(&self, model_id: &str) -> Option<String> {
        let mut guard = self
            .active_model_id
            .lock()
            .expect("local active-model lock poisoned");
        guard.replace(model_id.to_owned())
    }
}

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
    /// (see [`model_default`]).
    pub default_model: String,
}

/// Resolves the default model for a `(runner, tier)` pair, honouring an env
/// override so newly released models don't require a recompile:
///
/// ```text
/// SMEDJA_MODEL_<RUNNER>_<TIER>   e.g.  SMEDJA_MODEL_CLAUDE_DEEP=claude-opus-5
/// ```
///
/// `<RUNNER>` is the runner name's first segment upper-cased (`claude-cli` →
/// `CLAUDE`, `codex-cli` → `CODEX`); `<TIER>` is `FAST` | `DEEP` | `LOCAL`.
/// Falls back to `builtin` when the env var is unset or blank.
#[must_use]
pub fn model_default(runner_name: &str, tier: Tier, builtin: &str) -> String {
    let runner_key = runner_name
        .split('-')
        .next()
        .unwrap_or(runner_name)
        .to_ascii_uppercase();
    let tier_key = match tier {
        Tier::Fast => "FAST",
        Tier::Deep => "DEEP",
        Tier::Local => "LOCAL",
    };
    let env_key = format!("SMEDJA_MODEL_{runner_key}_{tier_key}");
    std::env::var(&env_key)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| builtin.to_owned())
}

/// Map from `(Runner, Tier)` to a concrete provider instance.
///
/// Built once at daemon start-up; shared across all concurrent turns via
/// `Arc<ProviderPool>`.
pub struct ProviderPool {
    entries: HashMap<(Runner, Tier), ProviderEntry>,
    /// Keys in stable insertion/priority order — the same order
    /// [`build_provider_pool`] probes providers, which determines the default
    /// and the rotation-ring priority. A `HashMap` does not preserve insertion
    /// order, so the ordering is tracked explicitly here.
    order: Vec<(Runner, Tier)>,
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

/// Probes all available providers and returns a populated pool.
///
/// The priority order matches the original `build_provider()` function so that
/// the pool default is the highest-priority available provider.  An empty pool
/// (all probes failed) is valid; callers handle `None` from `get()`.
///
/// # Panics
///
/// Panics if a provider binary that was just confirmed available via its
/// `available()` probe fails to re-detect immediately afterwards, which would
/// indicate the binary vanished mid-probe.
#[allow(clippy::too_many_lines)] // sequential provider probes kept inline; each branch logs a distinct readiness signal
pub async fn build_provider_pool() -> ProviderPool {
    let mut entries: HashMap<(Runner, Tier), ProviderEntry> = HashMap::new();
    let mut order: Vec<(Runner, Tier)> = Vec::new();
    let mut default: Option<(Runner, Tier)> = None;

    // Helper: record the first inserted (Runner, Tier) as the default and track
    // probe order so the rotation ring follows the pool's stable priority.
    macro_rules! add {
        ($runner:expr, $tier:expr, $provider:expr, $name:literal, $model:literal) => {{
            let key = ($runner, $tier);
            if default.is_none() {
                default = Some(key);
            }
            if entries
                .insert(
                    key,
                    ProviderEntry {
                        provider: Box::new($provider),
                        runner: $runner,
                        tier: $tier,
                        runner_name: $name,
                        // Built-in default, overridable via SMEDJA_MODEL_<RUNNER>_<TIER>.
                        default_model: model_default($name, $tier, $model),
                    },
                )
                .is_none()
            {
                order.push(key);
            }
        }};
    }

    // 1. Claude CLI (subscription — no API key)
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();
    if let Some(p) = ClaudeCliProvider::detect(None) {
        if SubprocessProvider::available("claude") {
            // Re-detect a second instance for the Deep tier.
            let p_deep = ClaudeCliProvider::detect(None);
            add!(
                Runner::Claude,
                Tier::Fast,
                p,
                "claude-cli",
                "claude-haiku-4-5-20251001"
            );
            if let Some(pd) = p_deep {
                add!(
                    Runner::Claude,
                    Tier::Deep,
                    pd,
                    "claude-cli",
                    "claude-opus-4-8"
                );
            }
            info!(runner = "claude-cli", "provider ready");
        }
    } else if let Some(key) = anthropic_key {
        let p_fast = AnthropicProvider::new(key.clone());
        let p_deep = AnthropicProvider::new(key);
        add!(
            Runner::Claude,
            Tier::Fast,
            p_fast,
            "anthropic",
            "claude-haiku-4-5-20251001"
        );
        add!(
            Runner::Claude,
            Tier::Deep,
            p_deep,
            "anthropic",
            "claude-sonnet-4-6"
        );
        info!(runner = "anthropic", "provider ready");
    } else {
        warn!(
            runner = "claude",
            "UNAVAILABLE — no claude binary and no ANTHROPIC_API_KEY"
        );
    }

    // 2. Codex CLI
    if SubprocessProvider::available("codex") {
        let p_fast = CodexCliProvider::detect(None).expect("codex binary just checked");
        add!(Runner::Codex, Tier::Fast, p_fast, "codex-cli", "gpt-5.5");
        info!(runner = "codex-cli", "provider ready");
    } else if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        let p = OpenAiProvider::new("https://api.openai.com", key);
        add!(Runner::Codex, Tier::Fast, p, "openai", "gpt-5.5");
        info!(runner = "openai", "provider ready");
    } else {
        warn!(
            runner = "codex",
            "UNAVAILABLE — no codex binary and no OPENAI_API_KEY"
        );
    }

    // 3. Copilot
    if let Some(p) = CopilotProvider::detect() {
        add!(Runner::Copilot, Tier::Fast, p, "copilot", "gpt-5.5");
        info!(runner = "copilot", "provider ready");
    }

    // 4. Poolside
    if let Some(p) = PoolsideProvider::detect() {
        add!(Runner::Copilot, Tier::Deep, p, "poolside", "poolside-muse");
        info!(runner = "poolside", "provider ready");
    }

    // 5. Minimax
    if let Some(p) = MinimaxProvider::detect() {
        add!(Runner::Local, Tier::Fast, p, "minimax", "MiniMax-M2");
        info!(runner = "minimax", "provider ready");
    }

    // 6. Berget
    if let Some(p) = BergetProvider::detect() {
        add!(Runner::Local, Tier::Local, p, "berget", "gpt-4o-mini");
        info!(runner = "berget", "provider ready");
    }

    // 7. Local rs-llmctl
    let local = LocalProvider::connect().await;
    let mut local_control: Option<LocalControl> = None;
    if local.capability.healthy {
        let active = local.capability.active_model_id.clone();
        info!(
            runner = "local",
            model_id = active.as_deref().unwrap_or(""),
            model_count = local.capability.inventory.len(),
            "provider ready",
        );
        // Capture the swap-proxy endpoint, full inventory, and a GPU snapshot for
        // the local control plane before the provider is boxed into the pool.
        local_control = Some(LocalControl::new(
            local.endpoint().to_owned(),
            local.swap_endpoint().to_owned(),
            local.capability.inventory.clone(),
            smedja_adapter::detect_gpu().await,
            active,
        ));
        add!(Runner::Local, Tier::Local, local, "local", "local");
    } else {
        warn!(runner = "local", "UNAVAILABLE — no local endpoint");
    }

    if entries.is_empty() {
        error!(
            "provider pool is EMPTY — no LLM provider is configured, so every turn will fail. \
             Set ANTHROPIC_API_KEY / OPENAI_API_KEY or a local endpoint and restart."
        );
    } else {
        info!(
            runners = ?entries.values().map(|e| e.runner_name).collect::<Vec<_>>(),
            default_runner = ?default.as_ref().and_then(|d| entries.get(d)).map(|e| e.runner_name),
            "provider pool built",
        );
    }

    ProviderPool {
        entries,
        order,
        default,
        local: local_control,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_default_uses_builtin_then_env_override() {
        // Use a unique runner key so the env var can't collide with a real one.
        let key = "SMEDJA_MODEL_ZZTEST_DEEP";
        std::env::remove_var(key);
        assert_eq!(
            model_default("zztest-cli", Tier::Deep, "builtin-x"),
            "builtin-x",
            "falls back to the built-in when unset"
        );
        std::env::set_var(key, "  ");
        assert_eq!(
            model_default("zztest", Tier::Deep, "builtin-x"),
            "builtin-x",
            "blank env override is ignored"
        );
        std::env::set_var(key, "future-model-9");
        assert_eq!(
            model_default("zztest-cli", Tier::Deep, "builtin-x"),
            "future-model-9",
            "env override wins so new models need no recompile"
        );
        std::env::remove_var(key);
    }

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
