//! Berget provider — `BERGET_API_KEY` HTTP adapter.

use crate::{CallOptions, DeltaStream, Message, OpenAiProvider, Provider};

/// OpenAI-compatible adapter for the Berget AI API.
pub struct BergetProvider(OpenAiProvider);

impl BergetProvider {
    /// Returns `Some(Self)` if `BERGET_API_KEY` is set in the environment.
    pub fn detect() -> Option<Self> {
        std::env::var("BERGET_API_KEY")
            .ok()
            .map(|key| Self(OpenAiProvider::new("https://api.berget.ai/v1", key)))
    }
}

impl Provider for BergetProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        self.0.stream_chat(messages, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_none_when_key_absent() {
        let saved = std::env::var("BERGET_API_KEY").ok();
        std::env::remove_var("BERGET_API_KEY");
        let provider = BergetProvider::detect();
        if let Some(v) = saved {
            std::env::set_var("BERGET_API_KEY", v);
        }
        assert!(provider.is_none());
    }

    #[test]
    fn detect_returns_some_when_key_present() {
        std::env::set_var("BERGET_API_KEY", "test-key");
        let provider = BergetProvider::detect();
        std::env::remove_var("BERGET_API_KEY");
        assert!(provider.is_some());
    }
}
