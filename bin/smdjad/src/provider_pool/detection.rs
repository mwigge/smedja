//! Provider detection: probes every available provider and assembles the pool.

use std::collections::HashMap;

use smedja_adapter::{
    AcpProvider, AnthropicProvider, BergetProvider, ClaudeCliProvider, CodexCliProvider,
    CopilotProvider, GeminiProvider, KimiCliProvider, KimiProvider, LocalProvider, MinimaxProvider,
    OpenAiProvider, PoolCliProvider, PoolsideProvider, SubprocessProvider, GEMINI_ACP,
};
use smedja_assayer::{Runner, Tier};
use tracing::{error, info, warn};

use super::pool::ProviderPool;
use super::types::{model_default, LocalControl, ProviderEntry};

/// Returns the preferred runner name for Claude given availability.
/// Native API wins over subprocess binary — API key users get native HTTP
/// without needing the `claude` CLI binary installed.
#[cfg(test)]
pub(crate) fn claude_preferred_runner(has_api_key: bool, has_binary: bool) -> Option<&'static str> {
    if has_api_key {
        Some("anthropic")
    } else if has_binary {
        Some("claude-cli")
    } else {
        None
    }
}

/// Returns the preferred runner name for Codex given availability.
/// Native API wins over subprocess binary — API key users get native HTTP
/// without needing the `codex` CLI binary installed.
#[cfg(test)]
pub(crate) fn codex_preferred_runner(has_api_key: bool, has_binary: bool) -> Option<&'static str> {
    if has_api_key {
        Some("openai")
    } else if has_binary {
        Some("codex-cli")
    } else {
        None
    }
}

/// Returns the preferred runner name for Kimi given availability.
/// Native API wins over subprocess binary — `MOONSHOT_API_KEY` users get
/// native HTTP without needing the `kimi` CLI binary installed.
#[cfg(test)]
pub(crate) fn kimi_preferred_runner(has_api_key: bool, has_binary: bool) -> Option<&'static str> {
    if has_api_key {
        Some("moonshot")
    } else if has_binary {
        Some("kimi-cli")
    } else {
        None
    }
}

/// Returns the preferred runner name for Gemini given availability.
/// Native API wins over subprocess binary — `GEMINI_API_KEY` users get
/// native HTTP without needing the `gemini` CLI binary installed.
#[cfg(test)]
pub(crate) fn gemini_preferred_runner(has_api_key: bool, has_binary: bool) -> Option<&'static str> {
    if has_api_key {
        Some("google")
    } else if has_binary {
        Some("gemini-cli")
    } else {
        None
    }
}

/// Probes all available providers and returns a populated pool.
///
/// The priority order matches the original `build_provider()` function so that
/// the pool default is the highest-priority available provider.  An empty pool
/// (all probes failed) is valid; callers handle `None` from `get()`.
///
/// A provider binary that passes its `available()` probe but fails to re-detect
/// immediately afterwards (a mid-probe TOCTOU where the binary vanished) is
/// logged and skipped rather than aborting pool construction.
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
        // TOCTOU: `available()` and `detect()` are separate probes, so a binary
        // that just passed `available()` can vanish before `detect()`. Skip the
        // provider on a `None` instead of panicking the daemon.
        if let Some(p) = ClaudeCliProvider::detect(None) {
            add!(
                Runner::Claude,
                Tier::Fast,
                p,
                "claude-cli",
                "claude-haiku-4-5-20251001"
            );
            if let Some(pd) = ClaudeCliProvider::detect(None) {
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
                runner = "claude-cli",
                "UNAVAILABLE — claude binary detected then vanished before probe"
            );
        }
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
        // Same detect TOCTOU as the claude branch: skip on `None`, never panic.
        if let Some(p_fast) = CodexCliProvider::detect(None) {
            add!(Runner::Codex, Tier::Fast, p_fast, "codex-cli", "gpt-5.5");
            info!(runner = "codex-cli", "provider ready");
        } else {
            warn!(
                runner = "codex-cli",
                "UNAVAILABLE — codex binary detected then vanished before probe"
            );
        }
    } else {
        warn!(
            runner = "codex",
            "UNAVAILABLE — no OPENAI_API_KEY and no codex binary"
        );
    }

    // 3. Kimi (Moonshot) — native API preferred; the kimi CLI binary is the
    //    fallback for Kimi Code subscription users (device-code OAuth) without
    //    a MOONSHOT_API_KEY.
    if let Some(p_fast) = KimiProvider::detect() {
        add!(
            Runner::Kimi,
            Tier::Fast,
            p_fast,
            "moonshot",
            "kimi-k2.7-code-highspeed"
        );
        if let Some(p_deep) = KimiProvider::detect() {
            add!(Runner::Kimi, Tier::Deep, p_deep, "moonshot", "kimi-k3");
        }
        info!(runner = "moonshot", "provider ready");
    } else if SubprocessProvider::available("kimi") {
        // Same detect TOCTOU as the claude branch: skip on `None`, never panic.
        if let Some(p_fast) = KimiCliProvider::detect() {
            add!(
                Runner::Kimi,
                Tier::Fast,
                p_fast,
                "kimi-cli",
                "kimi-code/kimi-for-coding-highspeed"
            );
            if let Some(p_deep) = KimiCliProvider::detect() {
                add!(Runner::Kimi, Tier::Deep, p_deep, "kimi-cli", "kimi-code/k3");
            }
            info!(runner = "kimi-cli", "provider ready");
        } else {
            warn!(
                runner = "kimi-cli",
                "UNAVAILABLE — kimi binary detected then vanished before probe"
            );
        }
    } else {
        warn!(
            runner = "kimi",
            "UNAVAILABLE — no MOONSHOT_API_KEY and no kimi binary"
        );
    }

    // 4. Gemini — native API preferred; the gemini CLI binary is the fallback,
    //    driven over ACP so its tool calls are gated like kimi's.
    if std::env::var("GEMINI_API_KEY").is_ok() {
        match (GeminiProvider::from_env(), GeminiProvider::from_env()) {
            (Ok(p_fast), Ok(p_deep)) => {
                add!(
                    Runner::Gemini,
                    Tier::Fast,
                    p_fast,
                    "google",
                    "gemini-2.5-flash"
                );
                add!(
                    Runner::Gemini,
                    Tier::Deep,
                    p_deep,
                    "google",
                    "gemini-2.5-pro"
                );
                info!(runner = "google", "provider ready");
            }
            _ => warn!(
                runner = "google",
                "UNAVAILABLE — GEMINI_API_KEY vanished mid-probe"
            ),
        }
    } else if SubprocessProvider::available("gemini") {
        // Same detect TOCTOU as the claude branch: skip on `None`, never panic.
        if let Some(p_fast) = AcpProvider::detect(GEMINI_ACP) {
            // Empty model literals: gemini's ACP mode uses the agent's own
            // configured default model; pins go via SMEDJA_MODEL_GEMINI_<TIER>.
            add!(Runner::Gemini, Tier::Fast, p_fast, "gemini-cli", "");
            if let Some(p_deep) = AcpProvider::detect(GEMINI_ACP) {
                add!(Runner::Gemini, Tier::Deep, p_deep, "gemini-cli", "");
            }
            info!(runner = "gemini-cli", "provider ready");
        } else {
            warn!(
                runner = "gemini-cli",
                "UNAVAILABLE — gemini binary detected then vanished before probe"
            );
        }
    } else {
        warn!(
            runner = "gemini",
            "UNAVAILABLE — no GEMINI_API_KEY and no gemini binary"
        );
    }

    // 5. Copilot
    if let Some(p) = CopilotProvider::detect() {
        add!(Runner::Copilot, Tier::Fast, p, "copilot", "gpt-5.5");
        info!(runner = "copilot", "provider ready");
    }

    // 6. Poolside
    if let Some(p) = PoolsideProvider::detect() {
        add!(Runner::Copilot, Tier::Deep, p, "poolside", "poolside-muse");
        info!(runner = "poolside", "provider ready");
    }

    // 7. Pool (Poolside `pool` CLI)
    if let Some(p) = PoolCliProvider::detect() {
        add!(Runner::Pool, Tier::Fast, p, "pool", "laguna-m1");
        info!(runner = "pool", "provider ready");
    }

    // 8. Minimax — keyed under its own runner so it is routable by name and does
    //    not shadow a local endpoint sharing the (Local, _) key space.
    if let Some(p) = MinimaxProvider::detect() {
        add!(Runner::Minimax, Tier::Fast, p, "minimax", "MiniMax-M2");
        info!(runner = "minimax", "provider ready");
    }

    // 9. Berget — keyed under Runner::Berget. Registering it under Runner::Local
    //    collided with the local rs-llmctl endpoint at (Local, Local): whichever
    //    probed second overwrote the other, so a healthy local endpoint made
    //    Berget dead config. Its own runner key lets both coexist.
    if let Some(p) = BergetProvider::detect() {
        add!(Runner::Berget, Tier::Local, p, "berget", "gpt-4o-mini");
        info!(runner = "berget", "provider ready");
    }

    // 10. Local rs-llmctl
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
