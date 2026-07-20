//! Unified OpenAI-compatible HTTP provider.
//!
//! Several upstream services (Minimax, Berget, `OpenCode`) expose the same
//! `OpenAI` chat-completions wire protocol and differ only by the environment
//! variable that holds the API key and the base URL.  Rather than maintain a
//! near-identical newtype per service, [`OpenAiCompatProvider`] captures that
//! variation as data and delegates streaming to a wrapped [`OpenAiProvider`].
//!
//! The per-service entry points are exposed as the marker constructors
//! [`MinimaxProvider`], [`BergetProvider`] and [`OpenCodeProvider`], each of
//! which provides a `detect()` returning an [`OpenAiCompatProvider`] when the
//! relevant API key is present in the environment.

use crate::{CallOptions, DeltaStream, Message, OpenAiProvider, Provider};

/// Description of an OpenAI-compatible service: which environment variable
/// holds its API key and which base URL its endpoint lives under.
#[derive(Debug, Clone, Copy)]
pub struct OpenAiCompatSpec {
    /// Environment variable that holds the API key.
    pub env_var: &'static str,
    /// Base URL of the service's OpenAI-compatible endpoint.
    pub base_url: &'static str,
}

/// Built-in spec for the Minimax API.
///
/// The base URL is the API root (without `/v1`); `OpenAiProvider` appends
/// `/v1/chat/completions` to it.
pub const MINIMAX: OpenAiCompatSpec = OpenAiCompatSpec {
    env_var: "MINIMAX_API_KEY",
    base_url: "https://api.minimax.io",
};

/// Built-in spec for the Kimi (Moonshot AI) API — the international endpoint.
///
/// The base URL is the API root (without `/v1`); `OpenAiProvider` appends
/// `/v1/chat/completions` to it. The mainland-China endpoint
/// (`https://api.moonshot.cn`) uses separate, non-interchangeable keys; select
/// it via the `MOONSHOT_BASE_URL` override.
pub const KIMI: OpenAiCompatSpec = OpenAiCompatSpec {
    env_var: "MOONSHOT_API_KEY",
    base_url: "https://api.moonshot.ai",
};

/// Built-in spec for the Berget AI API.
///
/// The base URL is the API root (without `/v1`); `OpenAiProvider` appends
/// `/v1/chat/completions` to it.
pub const BERGET: OpenAiCompatSpec = OpenAiCompatSpec {
    env_var: "BERGET_API_KEY",
    base_url: "https://api.berget.ai",
};

/// Built-in spec for the `OpenCode` API.
///
/// The base URL is the API root (without `/v1`); `OpenAiProvider` appends
/// `/v1/chat/completions` to it.
pub const OPENCODE: OpenAiCompatSpec = OpenAiCompatSpec {
    env_var: "OPENCODE_API_KEY",
    base_url: "https://api.opencode.ai",
};

/// A provider for any service that speaks the `OpenAI` chat-completions
/// protocol, parameterised by an [`OpenAiCompatSpec`].
pub struct OpenAiCompatProvider {
    spec: OpenAiCompatSpec,
    inner: OpenAiProvider,
}

impl OpenAiCompatProvider {
    /// Creates a provider for `spec` using the given API key.
    #[must_use]
    pub fn new(spec: OpenAiCompatSpec, api_key: impl Into<String>) -> Self {
        Self {
            spec,
            inner: OpenAiProvider::new(spec.base_url, api_key),
        }
    }

    /// Creates a provider for `spec` with an explicit base URL override.
    ///
    /// Used when a service permits redirecting its endpoint (e.g. staging).
    #[must_use]
    pub fn with_base_url(
        spec: OpenAiCompatSpec,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            spec,
            inner: OpenAiProvider::new(base_url, api_key),
        }
    }

    /// Returns `Some(Self)` when `spec.env_var` is set in the environment.
    #[must_use]
    pub fn detect(spec: OpenAiCompatSpec) -> Option<Self> {
        std::env::var(spec.env_var)
            .ok()
            .map(|key| Self::new(spec, key))
    }

    /// Returns the environment variable name this provider keys off.
    #[must_use]
    pub fn env_var(&self) -> &'static str {
        self.spec.env_var
    }

    /// Returns the configured base URL (which may be the spec default or an
    /// explicit override).
    #[must_use]
    pub fn base_url(&self) -> &str {
        self.inner.base_url()
    }
}

impl Provider for OpenAiCompatProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        self.inner.stream_chat(messages, opts)
    }
}

// ── Per-service entry points ──────────────────────────────────────────────────
//
// These zero-sized markers preserve the historical `Name::detect()` /
// `Name::new()` call sites while delegating to the unified provider.

/// Entry point for the Minimax API. Construct via [`MinimaxProvider::detect`].
pub struct MinimaxProvider;

impl MinimaxProvider {
    /// Returns `Some` when `MINIMAX_API_KEY` is set in the environment.
    #[must_use]
    pub fn detect() -> Option<OpenAiCompatProvider> {
        OpenAiCompatProvider::detect(MINIMAX)
    }
}

/// Entry point for the Kimi (Moonshot AI) API. Construct via
/// [`KimiProvider::detect`].
///
/// Kimi honours a `MOONSHOT_BASE_URL` override (e.g. to select the
/// mainland-China endpoint `https://api.moonshot.cn/v1`), and accepts the
/// key from `MOONSHOT_API_KEY` (official convention) or `KIMI_API_KEY`
/// (common third-party convention) — in that order.
pub struct KimiProvider;

impl KimiProvider {
    /// Returns `Some` when `MOONSHOT_API_KEY` (or `KIMI_API_KEY`) is set in
    /// the environment.
    ///
    /// The base URL defaults to `https://api.moonshot.ai/v1` but may be
    /// overridden via the `MOONSHOT_BASE_URL` environment variable.
    #[must_use]
    pub fn detect() -> Option<OpenAiCompatProvider> {
        let api_key = std::env::var(KIMI.env_var)
            .or_else(|_| std::env::var("KIMI_API_KEY"))
            .ok()?;
        match std::env::var("MOONSHOT_BASE_URL") {
            Ok(base_url) => Some(OpenAiCompatProvider::with_base_url(KIMI, base_url, api_key)),
            Err(_) => Some(OpenAiCompatProvider::new(KIMI, api_key)),
        }
    }
}

/// Entry point for the Berget AI API. Construct via [`BergetProvider::detect`].
pub struct BergetProvider;

impl BergetProvider {
    /// Returns `Some` when `BERGET_API_KEY` is set in the environment.
    #[must_use]
    pub fn detect() -> Option<OpenAiCompatProvider> {
        OpenAiCompatProvider::detect(BERGET)
    }
}

/// Entry point for the `OpenCode` API. Construct via [`OpenCodeProvider::detect`].
///
/// `OpenCode` additionally honours an `OPENCODE_BASE_URL` override; when set it
/// replaces the default base URL.
pub struct OpenCodeProvider;

impl OpenCodeProvider {
    /// Returns `Some` when `OPENCODE_API_KEY` is set in the environment.
    ///
    /// The base URL defaults to `https://api.opencode.ai/v1` but may be
    /// overridden via the `OPENCODE_BASE_URL` environment variable.
    #[must_use]
    pub fn detect() -> Option<OpenAiCompatProvider> {
        let api_key = std::env::var(OPENCODE.env_var).ok()?;
        match std::env::var("OPENCODE_BASE_URL") {
            Ok(base_url) => Some(OpenAiCompatProvider::with_base_url(
                OPENCODE, base_url, api_key,
            )),
            Err(_) => Some(OpenAiCompatProvider::new(OPENCODE, api_key)),
        }
    }

    /// Creates a provider with an explicit base URL and API key.
    #[must_use]
    pub fn with_base_url(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> OpenAiCompatProvider {
        OpenAiCompatProvider::with_base_url(OPENCODE, base_url, api_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_ENV_LOCK as ENV_LOCK;

    #[test]
    fn detect_returns_none_when_key_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var(MINIMAX.env_var).ok();
        std::env::remove_var(MINIMAX.env_var);
        let provider = OpenAiCompatProvider::detect(MINIMAX);
        if let Some(v) = saved {
            std::env::set_var(MINIMAX.env_var, v);
        }
        assert!(provider.is_none());
    }

    #[test]
    fn detect_returns_some_when_key_present() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(BERGET.env_var, "test-key");
        let provider = OpenAiCompatProvider::detect(BERGET);
        std::env::remove_var(BERGET.env_var);
        assert!(provider.is_some());
    }

    #[test]
    fn new_records_spec_env_var() {
        let provider = OpenAiCompatProvider::new(OPENCODE, "test-key");
        assert_eq!(provider.env_var(), "OPENCODE_API_KEY");
    }

    #[test]
    fn with_base_url_accepts_override() {
        let provider =
            OpenAiCompatProvider::with_base_url(OPENCODE, "https://staging.opencode.ai/v1", "k");
        // Construction must not panic; the spec is still recorded.
        assert_eq!(provider.env_var(), "OPENCODE_API_KEY");
    }

    #[test]
    fn minimax_marker_detect_returns_none_when_key_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("MINIMAX_API_KEY").ok();
        std::env::remove_var("MINIMAX_API_KEY");
        let provider = MinimaxProvider::detect();
        if let Some(v) = saved {
            std::env::set_var("MINIMAX_API_KEY", v);
        }
        assert!(provider.is_none());
    }

    #[test]
    fn minimax_marker_detect_returns_some_when_key_present() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("MINIMAX_API_KEY", "test-key");
        let provider = MinimaxProvider::detect();
        std::env::remove_var("MINIMAX_API_KEY");
        assert!(provider.is_some());
    }

    #[test]
    fn kimi_marker_detect_returns_none_when_keys_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved_moonshot = std::env::var("MOONSHOT_API_KEY").ok();
        let saved_kimi = std::env::var("KIMI_API_KEY").ok();
        std::env::remove_var("MOONSHOT_API_KEY");
        std::env::remove_var("KIMI_API_KEY");
        let provider = KimiProvider::detect();
        if let Some(v) = saved_moonshot {
            std::env::set_var("MOONSHOT_API_KEY", v);
        }
        if let Some(v) = saved_kimi {
            std::env::set_var("KIMI_API_KEY", v);
        }
        assert!(provider.is_none());
    }

    #[test]
    fn kimi_marker_detect_returns_some_when_moonshot_key_present() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("MOONSHOT_API_KEY", "test-key");
        std::env::remove_var("MOONSHOT_BASE_URL");
        let provider = KimiProvider::detect();
        std::env::remove_var("MOONSHOT_API_KEY");
        assert!(provider.is_some());
    }

    #[test]
    fn kimi_marker_detect_accepts_kimi_api_key_fallback() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("MOONSHOT_API_KEY").ok();
        std::env::remove_var("MOONSHOT_API_KEY");
        std::env::set_var("KIMI_API_KEY", "test-key");
        let provider = KimiProvider::detect();
        std::env::remove_var("KIMI_API_KEY");
        if let Some(v) = saved {
            std::env::set_var("MOONSHOT_API_KEY", v);
        }
        assert!(provider.is_some());
    }

    #[test]
    fn kimi_marker_detect_uses_custom_base_url_from_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("MOONSHOT_API_KEY", "test-key");
        // The override should be the API root (without trailing `/v1`); the
        // provider appends `/v1/chat/completions` internally.
        std::env::set_var("MOONSHOT_BASE_URL", "https://api.moonshot.cn");
        let provider = KimiProvider::detect();
        std::env::remove_var("MOONSHOT_API_KEY");
        std::env::remove_var("MOONSHOT_BASE_URL");
        let provider = provider.expect("key set → provider");
        assert_eq!(provider.env_var(), "MOONSHOT_API_KEY");
        assert_eq!(provider.base_url(), "https://api.moonshot.cn");
    }

    #[test]
    fn berget_marker_detect_returns_none_when_key_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("BERGET_API_KEY").ok();
        std::env::remove_var("BERGET_API_KEY");
        let provider = BergetProvider::detect();
        if let Some(v) = saved {
            std::env::set_var("BERGET_API_KEY", v);
        }
        assert!(provider.is_none());
    }

    #[test]
    fn berget_marker_detect_returns_some_when_key_present() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("BERGET_API_KEY", "test-key");
        let provider = BergetProvider::detect();
        std::env::remove_var("BERGET_API_KEY");
        assert!(provider.is_some());
    }

    #[test]
    fn opencode_marker_detect_returns_none_when_key_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var("OPENCODE_API_KEY").ok();
        std::env::remove_var("OPENCODE_API_KEY");
        let provider = OpenCodeProvider::detect();
        if let Some(v) = saved {
            std::env::set_var("OPENCODE_API_KEY", v);
        }
        assert!(provider.is_none());
    }

    #[test]
    fn opencode_marker_detect_uses_default_base_url_when_env_not_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::remove_var("OPENCODE_BASE_URL");
        let provider = OpenCodeProvider::detect();
        std::env::remove_var("OPENCODE_API_KEY");
        assert!(provider.is_some());
    }

    #[test]
    fn opencode_marker_detect_uses_custom_base_url_from_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        // Override must be the API root (without trailing `/v1`).
        std::env::set_var("OPENCODE_BASE_URL", "https://custom.opencode.example");
        let provider = OpenCodeProvider::detect();
        std::env::remove_var("OPENCODE_API_KEY");
        std::env::remove_var("OPENCODE_BASE_URL");
        let provider = provider.expect("key set → provider");
        assert_eq!(provider.base_url(), "https://custom.opencode.example");
    }

    #[test]
    fn opencode_marker_with_base_url_constructs_with_explicit_url() {
        let provider = OpenCodeProvider::with_base_url("https://staging.opencode.ai", "test-key");
        assert_eq!(provider.base_url(), "https://staging.opencode.ai");
    }

    // OpenAI-compatible providers delegate streaming (and therefore non-success
    // HTTP classification) to the wrapped `OpenAiProvider`, which routes through
    // `classify_http_error`. These assertions pin the quota / context-length
    // mappings the compat path relies on.

    #[test]
    fn compat_quota_response_maps_to_quota_exhausted() {
        let err = crate::classify_http_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"type":"insufficient_quota"}}"#,
        );
        assert_eq!(err.kind(), "quota_exhausted");
        assert!(err.is_retryable());
    }

    #[test]
    fn compat_context_length_response_maps_to_context_length_exceeded() {
        let err = crate::classify_http_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":{"code":"context_length_exceeded"}}"#,
        );
        assert_eq!(err.kind(), "context_length_exceeded");
        assert!(err.is_retryable());
    }
}
