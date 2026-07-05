//! Daemon-side loader and resolver for the `[embedder]` config block.
//!
//! Resolves the `[embedder]` block from `<workspace>/.smedja/config.toml`,
//! mirroring [`crate::methodology_config::load_methodology_config`]: a missing or
//! unparseable file resolves to the FNV default and never blocks startup. The
//! resolved config then drives [`resolve_embedder`], which probes a configured
//! learned endpoint and degrades to the FNV backend when it is unreachable.
//!
//! # Recall quality: prefer a local semantic endpoint
//!
//! The FNV bag-of-words backend is the *offline safety net*, not a good default
//! for recall — it is lexical (keyword overlap), so "semantic" recall over it is
//! keyword search. The **recommended** production configuration points at a
//! local, OpenAI-compatible embeddings server (e.g. a bge-small / MiniLM served
//! by llama.cpp, Ollama, or text-embeddings-inference on localhost):
//!
//! ```toml
//! # <workspace>/.smedja/config.toml
//! [embedder]
//! backend  = "learned"
//! endpoint = "http://127.0.0.1:9090"   # local /v1/embeddings server
//! model    = "bge-small-en-v1.5"
//! dim      = 384
//! ```
//!
//! With this set, recall is genuinely semantic; FNV only takes over if the
//! endpoint is unreachable, and that degradation is now *surfaced* (a WARN plus
//! [`Embedder::status`](crate::embedder_port::Embedder::status)) rather than
//! silent.
//!
//! # Seam: bundling a local model next
//!
//! The clean next step is an in-process semantic embedder (a bge-small / MiniLM
//! via `candle` or an ONNX runtime) so semantic recall needs no sidecar server.
//! It was deliberately *not* bundled here: the model weights (~90 MB) plus a
//! candle/ONNX + tokenizer dependency stack are too heavy and supply-chain-risky
//! to add well in a single pass. The seam is already in place — implement
//! [`Embedder`](crate::embedder_port::Embedder) for the bundled model (its
//! `status().semantic` returns `true`), then add a `EmbedderBackend::Local` arm
//! to [`resolve_embedder`] that constructs it and falls back to
//! [`FnvEmbedder`] on load failure, exactly as the learned arm falls back today.
//! No call site changes: every embedding flows through the resolved
//! `Arc<dyn Embedder>`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::embedder_port::{Embedder, FnvEmbedder, LearnedEmbedder};

/// Health-check deadline for a learned endpoint at startup, mirroring the
/// local-runner 500 ms pre-flight.
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_millis(500);

/// Which embedding backend the daemon should resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedderBackend {
    /// The FNV-1a bag-of-words default (offline, always available).
    #[default]
    Fnv,
    /// A learned `/v1/embeddings` backend selected by `endpoint`/`model`/`dim`.
    Learned,
}

/// Resolved `[embedder]` configuration.
///
/// Defaults to the FNV backend, so an absent block leaves the daemon on the
/// offline default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct EmbedderConfig {
    /// Selected backend (`fnv` | `learned`).
    pub backend: EmbedderBackend,
    /// Learned endpoint base URL (e.g. `http://127.0.0.1:9090`).
    pub endpoint: Option<String>,
    /// Learned model id sent in `/v1/embeddings` requests and tagged on rows.
    pub model: Option<String>,
    /// Vector dimension the learned model produces.
    pub dim: Option<usize>,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            backend: EmbedderBackend::Fnv,
            endpoint: None,
            model: None,
            dim: None,
        }
    }
}

/// The shape of `config.toml` we care about: just its optional `[embedder]` block.
#[derive(Debug, Deserialize, Default)]
struct ConfigFile {
    embedder: Option<EmbedderConfig>,
}

impl EmbedderConfig {
    /// Parses an [`EmbedderConfig`] from a `config.toml` string.
    ///
    /// # Errors
    ///
    /// Returns the [`toml::de::Error`] when the document cannot be parsed.
    pub fn from_toml_str(content: &str) -> Result<Self, toml::de::Error> {
        let file: ConfigFile = toml::from_str(content)?;
        Ok(file.embedder.unwrap_or_default())
    }
}

/// Resolves the [`EmbedderConfig`] for `workspace_root`.
///
/// Reads `<workspace_root>/.smedja/config.toml` when present. A missing file or
/// an unparseable one resolves to the FNV default, so the embedder is never
/// silently dropped because of config trouble.
#[must_use]
pub fn load_embedder_config(workspace_root: &Path) -> EmbedderConfig {
    let config_path = workspace_root.join(".smedja").join("config.toml");
    let Ok(content) = std::fs::read_to_string(&config_path) else {
        return EmbedderConfig::default();
    };
    match EmbedderConfig::from_toml_str(&content) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(error = %e, path = %config_path.display(), "invalid [embedder] config; using FNV default");
            EmbedderConfig::default()
        }
    }
}

/// Resolves a single `Arc<dyn Embedder>` from `config` and runtime availability.
///
/// - `backend = "fnv"` (or any incomplete learned config) resolves the
///   [`FnvEmbedder`] default.
/// - `backend = "learned"` resolves the [`LearnedEmbedder`] only when its
///   endpoint passes a startup health check; otherwise it falls back to the FNV
///   default (Decision 2/4). This never blocks startup and never errors.
pub async fn resolve_embedder(config: &EmbedderConfig) -> Arc<dyn Embedder> {
    if config.backend != EmbedderBackend::Learned {
        return Arc::new(FnvEmbedder::new());
    }

    let (Some(endpoint), Some(model), Some(dim)) = (&config.endpoint, &config.model, config.dim)
    else {
        tracing::warn!(
            "learned embedder selected but endpoint/model/dim incomplete; using FNV default"
        );
        return Arc::new(FnvEmbedder::new());
    };

    if learned_endpoint_healthy(endpoint).await {
        tracing::info!(endpoint, model, dim, "learned embedder resolved");
        Arc::new(LearnedEmbedder::new(endpoint.clone(), model.clone(), dim))
    } else {
        tracing::warn!(
            endpoint,
            "learned endpoint failed startup health check; using FNV default"
        );
        Arc::new(FnvEmbedder::new())
    }
}

/// Probes `GET {endpoint}/v1/models` with a short deadline, mirroring the
/// local-runner health check. Any non-success or transport error is unhealthy.
async fn learned_endpoint_healthy(endpoint: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(HEALTH_CHECK_TIMEOUT)
        .build()
    else {
        return false;
    };
    let url = format!("{}/v1/models", endpoint.trim_end_matches('/'));
    matches!(client.get(&url).send().await, Ok(resp) if resp.status().is_success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder_port::FNV_MODEL_ID;

    fn write_config(dir: &Path, body: &str) {
        let smedja = dir.join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(smedja.join("config.toml"), body).unwrap();
    }

    #[test]
    fn missing_config_resolves_to_fnv_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_embedder_config(dir.path());
        assert_eq!(cfg.backend, EmbedderBackend::Fnv);
    }

    #[test]
    fn unparseable_config_resolves_to_fnv_default() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "[embedder\nbackend = ");
        let cfg = load_embedder_config(dir.path());
        assert_eq!(cfg.backend, EmbedderBackend::Fnv);
    }

    #[test]
    fn fnv_backend_block_is_read() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "[embedder]\nbackend = \"fnv\"\n");
        let cfg = load_embedder_config(dir.path());
        assert_eq!(cfg.backend, EmbedderBackend::Fnv);
    }

    #[test]
    fn learned_backend_block_is_read() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            "[embedder]\nbackend = \"learned\"\nendpoint = \"http://127.0.0.1:9090\"\nmodel = \"minilm-l6-v2\"\ndim = 384\n",
        );
        let cfg = load_embedder_config(dir.path());
        assert_eq!(cfg.backend, EmbedderBackend::Learned);
        assert_eq!(cfg.endpoint.as_deref(), Some("http://127.0.0.1:9090"));
        assert_eq!(cfg.model.as_deref(), Some("minilm-l6-v2"));
        assert_eq!(cfg.dim, Some(384));
    }

    #[tokio::test]
    async fn fnv_config_resolves_to_fnv_embedder() {
        let cfg = EmbedderConfig::default();
        let e = resolve_embedder(&cfg).await;
        assert_eq!(e.model_id(), FNV_MODEL_ID);
        assert_eq!(e.dim(), 128);
    }

    #[tokio::test]
    async fn learned_config_with_unreachable_endpoint_resolves_to_fnv() {
        // Port 1 refuses connections immediately → health check fails.
        let cfg = EmbedderConfig {
            backend: EmbedderBackend::Learned,
            endpoint: Some("http://127.0.0.1:1".to_owned()),
            model: Some("minilm-l6-v2".to_owned()),
            dim: Some(384),
        };
        let e = resolve_embedder(&cfg).await;
        assert_eq!(
            e.model_id(),
            FNV_MODEL_ID,
            "an unreachable learned endpoint must resolve to the FNV default"
        );
    }

    #[tokio::test]
    async fn incomplete_learned_config_resolves_to_fnv() {
        let cfg = EmbedderConfig {
            backend: EmbedderBackend::Learned,
            endpoint: None,
            model: None,
            dim: None,
        };
        let e = resolve_embedder(&cfg).await;
        assert_eq!(e.model_id(), FNV_MODEL_ID);
    }
}
