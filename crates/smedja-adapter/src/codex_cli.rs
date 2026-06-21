//! Codex CLI provider — wraps [`SubprocessProvider`] for the `codex` binary.

use crate::{CallOptions, DeltaStream, Message, OpenAiProvider, Provider, SubprocessProvider};

/// Runs the `codex` CLI binary if available; falls back to [`OpenAiProvider`].
pub enum CodexCliProvider {
    /// Delegates to the locally installed `codex` CLI binary.
    Cli(SubprocessProvider),
    /// Delegates to the `OpenAI` HTTP API using an API key.
    Api(OpenAiProvider),
}

impl CodexCliProvider {
    /// Selects CLI if the `codex` binary is on `$PATH`, otherwise uses the API key.
    ///
    /// Returns `None` if neither is available.
    pub fn detect(api_key: Option<String>) -> Option<Self> {
        if SubprocessProvider::available("codex") {
            Some(Self::Cli(SubprocessProvider::new("codex", vec![])))
        } else {
            api_key.map(|key| Self::Api(OpenAiProvider::new("https://api.openai.com", key)))
        }
    }
}

impl Provider for CodexCliProvider {
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
        let provider = CodexCliProvider::detect(None);
        if !SubprocessProvider::available("codex") {
            assert!(provider.is_none());
        } else {
            assert!(matches!(provider, Some(CodexCliProvider::Cli(_))));
        }
    }

    #[test]
    fn detect_returns_api_when_no_binary_but_key_present() {
        if SubprocessProvider::available("codex") {
            return;
        }
        let provider = CodexCliProvider::detect(Some("test-key".into()));
        assert!(matches!(provider, Some(CodexCliProvider::Api(_))));
    }
}
