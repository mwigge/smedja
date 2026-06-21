//! Claude CLI provider — wraps [`SubprocessProvider`] for the `claude` binary.

use crate::{AnthropicProvider, CallOptions, DeltaStream, Message, Provider, SubprocessProvider};

/// Runs the `claude` CLI binary if available; falls back to [`AnthropicProvider`].
pub enum ClaudeCliProvider {
    /// Delegates to the locally installed `claude` CLI binary.
    Cli(SubprocessProvider),
    /// Delegates to the Anthropic HTTP API using an API key.
    Api(AnthropicProvider),
}

impl ClaudeCliProvider {
    /// Selects CLI if the `claude` binary is on `$PATH`, otherwise uses the API key.
    ///
    /// Returns `None` if neither is available.
    pub fn detect(api_key: Option<String>) -> Option<Self> {
        if SubprocessProvider::available("claude") {
            Some(Self::Cli(SubprocessProvider::new(
                "claude",
                vec!["-p".into()],
            )))
        } else {
            api_key.map(|key| Self::Api(AnthropicProvider::new(key)))
        }
    }
}

impl Provider for ClaudeCliProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        match self {
            Self::Cli(p) => p.stream_chat(messages, opts),
            Self::Api(p) => p.stream_chat(messages, opts),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_none_when_no_binary_and_no_key() {
        // Assumes `claude` is not on $PATH in CI. If it is, this picks Cli instead.
        let provider = ClaudeCliProvider::detect(None);
        if !SubprocessProvider::available("claude") {
            assert!(provider.is_none());
        } else {
            assert!(matches!(provider, Some(ClaudeCliProvider::Cli(_))));
        }
    }

    #[test]
    fn detect_returns_api_when_no_binary_but_key_present() {
        if SubprocessProvider::available("claude") {
            // CLI wins over API key; skip this case.
            return;
        }
        let provider = ClaudeCliProvider::detect(Some("test-key".into()));
        assert!(matches!(provider, Some(ClaudeCliProvider::Api(_))));
    }
}
