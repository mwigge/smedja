//! Minimax provider — `MINIMAX_API_KEY` HTTP adapter.

use crate::{CallOptions, DeltaStream, Message, OpenAiProvider, Provider};

/// OpenAI-compatible adapter for the Minimax API.
pub struct MinimaxProvider(OpenAiProvider);

impl MinimaxProvider {
    /// Returns `Some(Self)` if `MINIMAX_API_KEY` is set in the environment.
    #[must_use]
    pub fn detect() -> Option<Self> {
        std::env::var("MINIMAX_API_KEY")
            .ok()
            .map(|key| Self(OpenAiProvider::new("https://api.minimax.io/v1", key)))
    }
}

impl Provider for MinimaxProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        self.0.stream_chat(messages, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_none_when_key_absent() {
        let saved = std::env::var("MINIMAX_API_KEY").ok();
        std::env::remove_var("MINIMAX_API_KEY");
        let provider = MinimaxProvider::detect();
        if let Some(v) = saved {
            std::env::set_var("MINIMAX_API_KEY", v);
        }
        assert!(provider.is_none());
    }

    #[test]
    fn detect_returns_some_when_key_present() {
        std::env::set_var("MINIMAX_API_KEY", "test-key");
        let provider = MinimaxProvider::detect();
        std::env::remove_var("MINIMAX_API_KEY");
        assert!(provider.is_some());
    }
}
