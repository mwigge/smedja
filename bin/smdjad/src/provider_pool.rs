//! Runtime pool of available LLM providers, indexed by (Runner, Tier).

use std::collections::HashMap;

use smedja_adapter::{
    AnthropicProvider, BergetProvider, ClaudeCliProvider, CodexCliProvider, CopilotProvider,
    LocalProvider, MinimaxProvider, OpenAiProvider, PoolsideProvider, Provider, SubprocessProvider,
};
use smedja_assayer::{Runner, Tier};
use tracing::{info, warn};

/// A single pool entry: the provider plus the strings needed for logging and
/// session-store keying.
pub struct ProviderEntry {
    pub provider: Box<dyn Provider>,
    /// Short identifier used in logs and the session-resume store key.
    pub runner_name: &'static str,
    /// Default model name when no `SMEDJA_MODEL` env var or session override is set.
    pub default_model: &'static str,
}

/// Map from `(Runner, Tier)` to a concrete provider instance.
///
/// Built once at daemon start-up; shared across all concurrent turns via
/// `Arc<ProviderPool>`.
pub struct ProviderPool {
    entries: HashMap<(Runner, Tier), ProviderEntry>,
    /// The `(Runner, Tier)` used when the assayer selects a route that has no
    /// entry in the pool.
    pub default: Option<(Runner, Tier)>,
}

impl ProviderPool {
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
    pub fn default_model(&self) -> &'static str {
        self.default
            .as_ref()
            .and_then(|d| self.entries.get(d))
            .map_or("", |e| e.default_model)
    }

    /// Returns the default pool entry, or `None` when the pool is empty.
    #[must_use]
    pub fn get_default(&self) -> Option<&ProviderEntry> {
        self.default.as_ref().and_then(|d| self.entries.get(d))
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
    pub fn list_all_entries(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        let tier_str = |t: &Tier| match t {
            Tier::Fast => "fast",
            Tier::Deep => "deep",
            Tier::Local => "local",
        };
        let mut out: Vec<_> = self
            .entries
            .iter()
            .map(|((_, tier), entry)| (entry.runner_name, tier_str(tier), entry.default_model))
            .collect();
        out.sort_by_key(|&(runner, tier, _)| (runner, tier));
        out
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
    let mut default: Option<(Runner, Tier)> = None;

    // Helper: record the first inserted (Runner, Tier) as the default.
    macro_rules! add {
        ($runner:expr, $tier:expr, $provider:expr, $name:literal, $model:literal) => {{
            let key = ($runner, $tier);
            if default.is_none() {
                default = Some(key);
            }
            entries.insert(
                key,
                ProviderEntry {
                    provider: Box::new($provider),
                    runner_name: $name,
                    default_model: $model,
                },
            );
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
                    "claude-sonnet-4-6"
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
        add!(
            Runner::Codex,
            Tier::Fast,
            p_fast,
            "codex-cli",
            "gpt-4o-mini"
        );
        info!(runner = "codex-cli", "provider ready");
    } else if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        let p = OpenAiProvider::new("https://api.openai.com", key);
        add!(Runner::Codex, Tier::Fast, p, "openai", "gpt-4o-mini");
        info!(runner = "openai", "provider ready");
    } else {
        warn!(
            runner = "codex",
            "UNAVAILABLE — no codex binary and no OPENAI_API_KEY"
        );
    }

    // 3. Copilot
    if let Some(p) = CopilotProvider::detect() {
        add!(Runner::Copilot, Tier::Fast, p, "copilot", "gpt-4o-mini");
        info!(runner = "copilot", "provider ready");
    }

    // 4. Poolside
    if let Some(p) = PoolsideProvider::detect() {
        add!(Runner::Copilot, Tier::Deep, p, "poolside", "poolside-muse");
        info!(runner = "poolside", "provider ready");
    }

    // 5. Minimax
    if let Some(p) = MinimaxProvider::detect() {
        add!(Runner::Local, Tier::Fast, p, "minimax", "abab6.5s-chat");
        info!(runner = "minimax", "provider ready");
    }

    // 6. Berget
    if let Some(p) = BergetProvider::detect() {
        add!(Runner::Local, Tier::Local, p, "berget", "gpt-4o-mini");
        info!(runner = "berget", "provider ready");
    }

    // 7. Local rs-llmctl
    let local = LocalProvider::connect().await;
    if local.capability.healthy {
        info!(
            runner = "local",
            model_id = %local.capability.model_id,
            "provider ready",
        );
        add!(Runner::Local, Tier::Local, local, "local", "local");
    } else {
        warn!(runner = "local", "UNAVAILABLE — no local endpoint");
    }

    if entries.is_empty() {
        warn!("provider pool is empty — all turns will fail until a provider is configured");
    } else {
        info!(
            runners = ?entries.values().map(|e| e.runner_name).collect::<Vec<_>>(),
            default_runner = ?default.as_ref().and_then(|d| entries.get(d)).map(|e| e.runner_name),
            "provider pool built",
        );
    }

    ProviderPool { entries, default }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut default = None;
        for (key, runner_name, default_model) in entries {
            if default.is_none() {
                default = Some(key);
            }
            map.insert(
                key,
                ProviderEntry {
                    provider: Box::new(NullProvider),
                    runner_name,
                    default_model,
                },
            );
        }
        ProviderPool {
            entries: map,
            default,
        }
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
            default: None,
        };
        assert!(pool.get(Runner::Claude, Tier::Fast).is_none());
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
}
