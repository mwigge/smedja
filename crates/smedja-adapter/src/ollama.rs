//! Ollama first-class provider — OpenAI-compatible `/api/chat` endpoint.
//!
//! Ollama (<https://ollama.com>) exposes an OpenAI-compatible API at its `/api/chat`
//! endpoint.  This module wraps that into the standard [`Provider`] interface using
//! [`OpenAiProvider`] with a custom base URL.
//!
//! # Model listing
//!
//! [`OllamaProvider::list_models`] hits Ollama's own `GET /api/tags` endpoint and
//! returns the names of locally installed models.

use reqwest::Client;

use crate::{CallOptions, DeltaStream, Message, OpenAiProvider, Provider};

// ---------------------------------------------------------------------------
// M11a — Ollama provider
// ---------------------------------------------------------------------------

/// Default Ollama base URL when `OLLAMA_HOST` is not set.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// A provider that connects to a locally running Ollama instance.
///
/// Ollama exposes an OpenAI-compatible `/api/chat` endpoint, so the streaming
/// implementation delegates to [`OpenAiProvider`] pointed at the Ollama host.
pub struct OllamaProvider {
    /// Ollama host, e.g. `http://localhost:11434`.
    pub base_url: String,
    inner: OpenAiProvider,
}

impl OllamaProvider {
    /// Creates a provider pointing at `base_url` (no API key needed).
    ///
    /// Sets `gen_ai.system = "ollama"` on `OTel` spans so traces distinguish
    /// Ollama from `OpenAI` despite sharing the same wire protocol.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        // Ollama's OpenAI-compatible chat endpoint lives at /v1.
        let api_base = format!("{base_url}/v1");
        Self {
            base_url,
            inner: OpenAiProvider::new_with_system(api_base, "", "ollama"),
        }
    }

    /// Detects a running Ollama instance.
    ///
    /// Reads `OLLAMA_HOST` from the environment, falling back to
    /// [`DEFAULT_BASE_URL`].  Does **not** verify connectivity.
    #[must_use]
    pub fn detect() -> Self {
        let base_url = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
        Self::new(base_url)
    }

    /// Lists locally installed Ollama models by calling `GET /api/tags`.
    ///
    /// Returns the model names, or an empty `Vec` when the request fails or
    /// Ollama is not running.
    pub async fn list_models(&self) -> Vec<String> {
        let url = format!("{}/api/tags", self.base_url);
        let Ok(resp) = Client::new().get(&url).send().await else {
            tracing::warn!(url = %url, "Ollama unreachable — list_models returning empty");
            return Vec::new();
        };
        let Ok(json) = resp.json::<serde_json::Value>().await else {
            return Vec::new();
        };
        json["models"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Provider for OllamaProvider {
    fn stream_chat(&self, messages: &[Message], opts: &CallOptions) -> DeltaStream {
        self.inner.stream_chat(messages, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_provider_base_url_default() {
        // When OLLAMA_HOST is absent, the default localhost URL is used.
        let saved = std::env::var("OLLAMA_HOST").ok();
        std::env::remove_var("OLLAMA_HOST");

        let provider = OllamaProvider::detect();
        assert_eq!(provider.base_url, DEFAULT_BASE_URL);
        assert!(
            provider.base_url.contains("localhost:11434"),
            "default base URL must point to localhost:11434; got: {}",
            provider.base_url
        );

        if let Some(v) = saved {
            std::env::set_var("OLLAMA_HOST", v);
        }
    }

    #[test]
    fn ollama_provider_custom_base_url() {
        let provider = OllamaProvider::new("http://my-gpu-box:11434");
        assert_eq!(provider.base_url, "http://my-gpu-box:11434");
    }

    #[test]
    fn ollama_provider_reports_ollama_as_system_name() {
        let provider = OllamaProvider::new("http://localhost:11434");
        // Traces must attribute to "ollama", not "openai".
        assert_eq!(provider.inner.system_name(), "ollama");
    }
}
