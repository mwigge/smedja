//! GitHub Copilot provider — CLI-first with `GITHUB_TOKEN` fallback.

use crate::{CallOptions, DeltaStream, Message, OpenAiProvider, Provider, SubprocessProvider};

/// Runs the `gh copilot suggest` CLI if available; falls back to the Copilot HTTP API.
pub enum CopilotProvider {
    /// Delegates to the locally installed `gh` CLI with the copilot extension.
    Cli(SubprocessProvider),
    /// Delegates to the GitHub Copilot HTTP API using a `GITHUB_TOKEN` bearer key.
    Api(OpenAiProvider),
}

impl CopilotProvider {
    /// Returns `Some(Self)` if either `gh` binary is on `$PATH` or `GITHUB_TOKEN` is set.
    pub fn detect() -> Option<Self> {
        if which::which("gh").is_ok() {
            return Some(Self::Cli(SubprocessProvider::new(
                "gh",
                vec![
                    "copilot".into(),
                    "suggest".into(),
                    "-t".into(),
                    "code".into(),
                    "-m".into(),
                ],
            )));
        }
        std::env::var("GITHUB_TOKEN")
            .ok()
            .map(|key| Self::Api(OpenAiProvider::new("https://api.githubcopilot.com", key)))
    }
}

impl Provider for CopilotProvider {
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
    fn detect_returns_cli_when_gh_on_path() {
        // This test is conditional: if `gh` is on PATH, Cli is selected.
        if which::which("gh").is_ok() {
            let provider = CopilotProvider::detect();
            assert!(matches!(provider, Some(CopilotProvider::Cli(_))));
        }
    }

    #[test]
    fn detect_returns_api_when_no_gh_but_token_set() {
        if which::which("gh").is_ok() {
            // CLI wins; skip this case.
            return;
        }
        // Temporarily set the env var to verify the fallback.
        std::env::set_var("GITHUB_TOKEN", "test-token");
        let provider = CopilotProvider::detect();
        std::env::remove_var("GITHUB_TOKEN");
        assert!(matches!(provider, Some(CopilotProvider::Api(_))));
    }

    #[test]
    fn detect_returns_none_when_no_gh_and_no_token() {
        if which::which("gh").is_ok() {
            // CLI wins; can't test the None path.
            return;
        }
        let saved = std::env::var("GITHUB_TOKEN").ok();
        std::env::remove_var("GITHUB_TOKEN");
        let provider = CopilotProvider::detect();
        if let Some(v) = saved {
            std::env::set_var("GITHUB_TOKEN", v);
        }
        assert!(provider.is_none());
    }
}
