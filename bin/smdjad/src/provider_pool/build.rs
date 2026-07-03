//! Startup provider probing that constructs the populated [`ProviderPool`].

use std::collections::HashMap;

use smedja_adapter::{
    AnthropicProvider, BergetProvider, ClaudeCliProvider, CodexCliProvider, CopilotProvider,
    DeepSeekProvider, GroqProvider, LocalProvider, MinimaxProvider, OllamaProvider, OpenAiProvider,
    PerplexityProvider, PoolCliProvider, PoolsideProvider, SubprocessProvider, TogetherProvider,
    XAiProvider,
};
use smedja_assayer::{Runner, Tier};
use tracing::{error, info, warn};

use crate::provider_pool::local_control::LocalControl;
use crate::provider_pool::model::model_default;
use crate::provider_pool::pool::{ProviderEntry, ProviderPool};

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

    // 1. Claude — native API preferred; CLI binary is the fallback for
    //    subscription users without an ANTHROPIC_API_KEY.
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();
    if let Some(key) = anthropic_key {
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
    } else if SubprocessProvider::available("claude") {
        let p = ClaudeCliProvider::detect(None).expect("claude binary just confirmed available");
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
    } else {
        warn!(
            runner = "claude",
            "UNAVAILABLE — no ANTHROPIC_API_KEY and no claude binary"
        );
    }

    // 2. Codex/OpenAI — native API preferred; CLI binary is the fallback.
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        let p = OpenAiProvider::new("https://api.openai.com", key);
        add!(Runner::Codex, Tier::Fast, p, "openai", "gpt-5.5");
        info!(runner = "openai", "provider ready");
    } else if SubprocessProvider::available("codex") {
        let p_fast = CodexCliProvider::detect(None).expect("codex binary just confirmed available");
        add!(Runner::Codex, Tier::Fast, p_fast, "codex-cli", "gpt-5.5");
        info!(runner = "codex-cli", "provider ready");
    } else {
        warn!(
            runner = "codex",
            "UNAVAILABLE — no OPENAI_API_KEY and no codex binary"
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

    // 5. Pool (Poolside `pool` CLI)
    if let Some(p) = PoolCliProvider::detect() {
        add!(Runner::Pool, Tier::Fast, p, "pool", "laguna-m1");
        info!(runner = "pool", "provider ready");
    }

    // 6. Minimax
    if let Some(p) = MinimaxProvider::detect() {
        add!(Runner::Local, Tier::Fast, p, "minimax", "MiniMax-M2");
        info!(runner = "minimax", "provider ready");
    }

    // 6. Berget
    if let Some(p) = BergetProvider::detect() {
        add!(Runner::Local, Tier::Local, p, "berget", "gpt-4o-mini");
        info!(runner = "berget", "provider ready");
    }

    // 7. Groq
    if let Some(p) = GroqProvider::detect() {
        add!(
            Runner::Codex,
            Tier::Fast,
            p,
            "groq",
            "llama-3.3-70b-versatile"
        );
        info!(runner = "groq", "provider ready");
    }

    // 8. DeepSeek
    if let Some(p) = DeepSeekProvider::detect() {
        add!(Runner::Codex, Tier::Deep, p, "deepseek", "deepseek-chat");
        info!(runner = "deepseek", "provider ready");
    }

    // 9. Together
    if let Some(p) = TogetherProvider::detect() {
        add!(
            Runner::Local,
            Tier::Fast,
            p,
            "together",
            "meta-llama/Llama-3-8b-chat-hf"
        );
        info!(runner = "together", "provider ready");
    }

    // 10. Perplexity
    if let Some(p) = PerplexityProvider::detect() {
        add!(Runner::Local, Tier::Fast, p, "perplexity", "sonar-pro");
        info!(runner = "perplexity", "provider ready");
    }

    // 11. xAI
    if let Some(p) = XAiProvider::detect() {
        add!(Runner::Local, Tier::Fast, p, "xai", "grok-3-beta");
        info!(runner = "xai", "provider ready");
    }

    // 12. Ollama — local inference; present when OLLAMA_HOST is set or the
    //     default port is reachable.  detect() never fails, so guard on the
    //     env var; connection failures surface only at turn time.
    if std::env::var("OLLAMA_HOST").is_ok() {
        let p = OllamaProvider::detect();
        add!(Runner::Local, Tier::Local, p, "ollama", "llama3");
        info!(runner = "ollama", "provider ready");
    }

    // 13. Local rs-llmctl
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
