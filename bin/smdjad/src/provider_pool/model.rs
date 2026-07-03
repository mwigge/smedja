//! Default-model resolution and runner-preference helpers.

use smedja_assayer::Tier;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate process-global env vars; cargo runs tests
    /// multithreaded, so env-mutating tests must hold this shared lock.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn model_default_uses_builtin_then_env_override() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

    #[test]
    fn native_api_preferred_over_subprocess_for_claude() {
        assert_eq!(
            claude_preferred_runner(true, true),
            Some("anthropic"),
            "API key wins over binary"
        );
        assert_eq!(
            claude_preferred_runner(false, true),
            Some("claude-cli"),
            "binary used when no key"
        );
        assert_eq!(
            claude_preferred_runner(true, false),
            Some("anthropic"),
            "key works without binary"
        );
        assert_eq!(claude_preferred_runner(false, false), None);
    }

    #[test]
    fn native_api_preferred_over_subprocess_for_codex() {
        assert_eq!(
            codex_preferred_runner(true, true),
            Some("openai"),
            "API key wins over binary"
        );
        assert_eq!(
            codex_preferred_runner(false, true),
            Some("codex-cli"),
            "binary used when no key"
        );
        assert_eq!(
            codex_preferred_runner(true, false),
            Some("openai"),
            "key works without binary"
        );
        assert_eq!(codex_preferred_runner(false, false), None);
    }
}
