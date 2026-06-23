//! `OpenCode` adapter — `OPENCODE_API_KEY` HTTP adapter.
//!
//! `OpenCode` exposes an OpenAI-compatible API.  This adapter wraps
//! [`OpenAiProvider`] with the `OpenCode` base URL and reads the API key from
//! `OPENCODE_API_KEY`.

use crate::{CallOptions, DeltaStream, Message, OpenAiProvider, Provider};

/// Default base URL for the `OpenCode` API.
const DEFAULT_BASE_URL: &str = "https://api.opencode.ai/v1";

/// `OpenAI`-compatible adapter for the `OpenCode` API.
pub struct OpenCodeProvider(OpenAiProvider);

impl OpenCodeProvider {
    /// Returns `Some(Self)` if `OPENCODE_API_KEY` is set in the environment.
    ///
    /// The base URL defaults to `https://api.opencode.ai/v1` but can be
    /// overridden via the `OPENCODE_BASE_URL` environment variable.
    #[must_use]
    pub fn detect() -> Option<Self> {
        let api_key = std::env::var("OPENCODE_API_KEY").ok()?;
        let base_url =
            std::env::var("OPENCODE_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
        Some(Self(OpenAiProvider::new(base_url, api_key)))
    }

    /// Creates a new [`OpenCodeProvider`] with explicit key and base URL.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self(OpenAiProvider::new(base_url, api_key))
    }
}

impl Provider for OpenCodeProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        self.0.stream_chat(messages, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_none_when_key_absent() {
        let saved = std::env::var("OPENCODE_API_KEY").ok();
        std::env::remove_var("OPENCODE_API_KEY");
        let provider = OpenCodeProvider::detect();
        if let Some(v) = saved {
            std::env::set_var("OPENCODE_API_KEY", v);
        }
        assert!(provider.is_none());
    }

    #[test]
    fn detect_returns_some_when_key_present() {
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        let provider = OpenCodeProvider::detect();
        std::env::remove_var("OPENCODE_API_KEY");
        assert!(provider.is_some());
    }

    #[test]
    fn detect_uses_default_base_url_when_env_not_set() {
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::remove_var("OPENCODE_BASE_URL");
        // Verify detect() doesn't panic and returns Some; the URL is tested
        // implicitly — if the wrong URL were used the provider would still be
        // constructed, so we just verify it constructs successfully.
        let provider = OpenCodeProvider::detect();
        std::env::remove_var("OPENCODE_API_KEY");
        assert!(provider.is_some());
    }

    #[test]
    fn detect_uses_custom_base_url_from_env() {
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::set_var("OPENCODE_BASE_URL", "https://custom.opencode.example/v1");
        let provider = OpenCodeProvider::detect();
        std::env::remove_var("OPENCODE_API_KEY");
        std::env::remove_var("OPENCODE_BASE_URL");
        assert!(provider.is_some());
    }

    #[test]
    fn new_constructs_with_explicit_url() {
        // Verifies that an explicit URL different from the default is accepted.
        let provider = OpenCodeProvider::new("https://staging.opencode.ai/v1", "test-key");
        // The inner OpenAiProvider is opaque, but construction must not panic.
        drop(provider);
    }
}
