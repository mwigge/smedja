//! Provider detection: probes every available provider and assembles the pool.

use std::collections::HashMap;

use smedja_adapter::{
    AcpProvider, AnthropicProvider, ClaudeCliProvider, CodexCliProvider, CopilotProvider,
    GeminiProvider, KimiCliProvider, LocalProvider, OpenAiCompatProvider, OpenAiProvider,
    PoolCliProvider, SubprocessProvider, BERGET, CEREBRAS, DEEPSEEK, GEMINI_ACP, GROQ, KIMI,
    MINIMAX, MISTRAL, OLLAMA_CLOUD, OPENROUTER, XAI,
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
    build_provider_pool_with_keys(&HashMap::new()).await
}

/// Build a replacement pool using saved credentials without changing process
/// environment while other daemon threads are running.
pub async fn build_provider_pool_with_keys(keys: &HashMap<String, String>) -> ProviderPool {
    let credential = |name: &str| keys.get(name).cloned().or_else(|| std::env::var(name).ok());
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
    let anthropic_key = credential("ANTHROPIC_API_KEY");
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
    if let Some(key) = credential("OPENAI_API_KEY") {
        let p = OpenAiProvider::new("https://api.openai.com", key.clone());
        add!(Runner::Codex, Tier::Fast, p, "openai", "gpt-5.5");
        // Deep tier uses the same latest model by default; override with
        // SMEDJA_MODEL_OPENAI_DEEP if a stronger model is available.
        let p_deep = OpenAiProvider::new("https://api.openai.com", key);
        add!(Runner::Codex, Tier::Deep, p_deep, "openai", "gpt-5.5");
        info!(runner = "openai", "provider ready");
    } else if SubprocessProvider::available("codex") {
        // Same detect TOCTOU as the claude branch: skip on `None`, never panic.
        if let Some(p_fast) = CodexCliProvider::detect(None) {
            add!(Runner::Codex, Tier::Fast, p_fast, "codex-cli", "gpt-5.5");
            // Deep tier uses the same latest model by default; override with
            // SMEDJA_MODEL_CODEX_DEEP if a stronger model is available.
            if let Some(p_deep) = CodexCliProvider::detect(None) {
                add!(Runner::Codex, Tier::Deep, p_deep, "codex-cli", "gpt-5.5");
            }
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
    if let Some(key) = credential("MOONSHOT_API_KEY") {
        let make_kimi = || {
            if let Ok(url) = std::env::var("MOONSHOT_BASE_URL") {
                let root = url.trim_end_matches('/').trim_end_matches("/v1");
                OpenAiCompatProvider::with_base_url(KIMI, root, key.clone())
            } else {
                OpenAiCompatProvider::new(KIMI, key.clone())
            }
        };
        let p_fast = make_kimi();
        add!(
            Runner::Kimi,
            Tier::Fast,
            p_fast,
            "moonshot",
            "kimi-k2.7-code-highspeed"
        );
        add!(Runner::Kimi, Tier::Deep, make_kimi(), "moonshot", "kimi-k3");
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

    // When a Moonshot Platform key and a Kimi Code subscription coexist,
    // retain a separately selectable gated CLI path instead of hiding it
    // behind the API-preferred `kimi` runner.
    if credential("MOONSHOT_API_KEY").is_some() && SubprocessProvider::available("kimi") {
        if let Some(p_fast) = KimiCliProvider::detect() {
            add!(
                Runner::KimiCode,
                Tier::Fast,
                p_fast,
                "kimi-code",
                "kimi-code/kimi-for-coding-highspeed"
            );
            if let Some(p_deep) = KimiCliProvider::detect() {
                add!(
                    Runner::KimiCode,
                    Tier::Deep,
                    p_deep,
                    "kimi-code",
                    "kimi-code/k3"
                );
            }
        }
    }

    // 4. Gemini — native API preferred; the gemini CLI binary is the fallback,
    //    driven over ACP so its tool calls are gated like kimi's.
    if let Some(key) = credential("GEMINI_API_KEY") {
        let p_fast = GeminiProvider::new(key.clone());
        let p_deep = GeminiProvider::new(key);
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
        add!(Runner::Copilot, Tier::Fast, p, "copilot", "");
        info!(runner = "copilot", "provider ready");
    }

    // 6. Pool (Poolside `pool` CLI)
    if let Some(p) = PoolCliProvider::detect() {
        add!(Runner::Pool, Tier::Fast, p, "pool", "laguna-m1");
        info!(runner = "pool", "provider ready");
    }

    // 8. Minimax — keyed under its own runner so it is routable by name and does
    //    not shadow a local endpoint sharing the (Local, _) key space.
    if let Some(p) =
        credential("MINIMAX_API_KEY").map(|key| OpenAiCompatProvider::new(MINIMAX, key))
    {
        add!(Runner::Minimax, Tier::Fast, p, "minimax", "MiniMax-M2");
        info!(runner = "minimax", "provider ready");
    }

    // 9. Berget — keyed under Runner::Berget. Registering it under Runner::Local
    //    collided with the local rs-llmctl endpoint at (Local, Local): whichever
    //    probed second overwrote the other, so a healthy local endpoint made
    //    Berget dead config. Its own runner key lets both coexist.
    if let Some(p) = credential("BERGET_API_KEY").map(|key| OpenAiCompatProvider::new(BERGET, key))
    {
        add!(Runner::Berget, Tier::Local, p, "berget", "gpt-4o-mini");
        info!(runner = "berget", "provider ready");
    }

    // Additional OpenAI-compatible suppliers. Each has its own runner identity
    // so selection and fallback never silently substitute another account.
    for (runner, spec, name, fast, deep) in [
        (
            Runner::Mistral,
            MISTRAL,
            "mistral",
            "mistral-small-latest",
            "mistral-large-latest",
        ),
        (
            Runner::Deepseek,
            DEEPSEEK,
            "deepseek",
            "deepseek-v4-flash",
            "deepseek-v4-pro",
        ),
        (
            Runner::OllamaCloud,
            OLLAMA_CLOUD,
            "ollama-cloud",
            "gpt-oss:20b",
            "gpt-oss:120b",
        ),
        (
            Runner::Openrouter,
            OPENROUTER,
            "openrouter",
            "~openai/gpt-latest",
            "~openai/gpt-latest",
        ),
        (Runner::Xai, XAI, "xai", "grok-4.3", "grok-4.6"),
        (
            Runner::Groq,
            GROQ,
            "groq",
            "openai/gpt-oss-20b",
            "openai/gpt-oss-120b",
        ),
        (
            Runner::Cerebras,
            CEREBRAS,
            "cerebras",
            "gpt-oss-120b",
            "gpt-oss-120b",
        ),
    ] {
        if let Some(api_key) = credential(spec.env_var) {
            let p_fast = OpenAiCompatProvider::new(spec, api_key.clone());
            // `add!` requires static model literals to feed model_default.
            // Build these entries directly for data-driven suppliers.
            for (tier, model, provider) in [
                (Tier::Fast, fast, p_fast),
                (
                    Tier::Deep,
                    deep,
                    OpenAiCompatProvider::new(spec, api_key.clone()),
                ),
            ] {
                let key = (runner, tier);
                entries.insert(
                    key,
                    ProviderEntry {
                        provider: Box::new(provider),
                        runner,
                        tier,
                        runner_name: name,
                        default_model: model_default(name, tier, model),
                    },
                );
                order.push(key);
                if default.is_none() {
                    default = Some(key);
                }
            }
            info!(runner = name, "provider ready");
        }
    }

    // A custom OpenAI-compatible endpoint is opt-in and must declare its model.
    if let (Some(key), Ok(base_url), Ok(model)) = (
        credential("SMEDJA_COMPAT_API_KEY"),
        std::env::var("SMEDJA_COMPAT_BASE_URL"),
        std::env::var("SMEDJA_COMPAT_MODEL"),
    ) {
        if valid_custom_base_url(&base_url) && !model.trim().is_empty() {
            let spec = smedja_adapter::OpenAiCompatSpec {
                env_var: "SMEDJA_COMPAT_API_KEY",
                base_url: "",
            };
            let root = base_url.trim_end_matches('/').trim_end_matches("/v1");
            for tier in [Tier::Fast, Tier::Deep] {
                let entry_key = (Runner::Custom, tier);
                entries.insert(
                    entry_key,
                    ProviderEntry {
                        provider: Box::new(OpenAiCompatProvider::with_base_url(
                            spec,
                            root,
                            key.clone(),
                        )),
                        runner: Runner::Custom,
                        tier,
                        runner_name: "custom",
                        default_model: model_default("custom", tier, &model),
                    },
                );
                order.push(entry_key);
                if default.is_none() {
                    default = Some(entry_key);
                }
            }
            info!(runner = "custom", "provider ready");
        } else {
            warn!(
                runner = "custom",
                "invalid custom endpoint or model; provider skipped"
            );
        }
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

fn valid_custom_base_url(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    url.scheme() == "https" || (url.scheme() == "http" && local)
}

#[cfg(test)]
mod custom_url_tests {
    use super::valid_custom_base_url;

    #[test]
    fn custom_url_requires_https_except_loopback() {
        assert!(valid_custom_base_url("https://api.example.org"));
        assert!(valid_custom_base_url("http://127.0.0.1:11434"));
        assert!(!valid_custom_base_url("http://api.example.org"));
        assert!(!valid_custom_base_url("file:///tmp/models"));
    }
}
