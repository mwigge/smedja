//! Daemon-side loader and resolver for the `[embedder]` config block.
//!
//! Resolves the `[embedder]` block from `<workspace>/.smedja/config.toml`,
//! mirroring [`crate::methodology_config::load_methodology_config`]: a missing or
//! unparseable file resolves to the FNV default and never blocks startup. The
//! resolved config then drives [`resolve_embedder`], which probes a configured
//! learned endpoint and degrades to the FNV backend when it is unreachable.
//!
//! # Recall quality: semantic by default
//!
//! The default backend is `local` — the bundled in-process semantic model
//! ([`crate::local_embedder`], all-MiniLM / bge-small, 384-dim), downloaded on
//! first use into `~/.local/share/smedja/models/`. Out of the box, vault recall
//! is genuinely semantic with no sidecar server. If the model cannot be fetched
//! or loaded (offline, uncached), it degrades to a *flagged* lexical FNV backend
//! ([`Embedder::status`](crate::embedder_port::Embedder::status) reports
//! `semantic = false, degraded = true`) rather than silently becoming keyword
//! search.
//!
//! Two other backends are selectable:
//!
//! ```toml
//! # <workspace>/.smedja/config.toml
//!
//! # Explicit lexical-only, no download:
//! [embedder]
//! backend = "fnv"
//!
//! # Or an external OpenAI-compatible embeddings server:
//! [embedder]
//! backend  = "learned"
//! endpoint = "http://127.0.0.1:9090"   # local /v1/embeddings server
//! model    = "bge-small-en-v1.5"
//! dim      = 384
//! ```
//!
//! Every embedding flows through the resolved `Arc<dyn Embedder>`, so the backend
//! choice and the `model_id`/`dim` pairing live entirely in [`resolve_embedder`];
//! no call site changes when the backend changes.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::embedder_port::{Embedder, FnvEmbedder, LearnedEmbedder, LocalFallbackEmbedder};

/// Health-check deadline for a learned endpoint at startup, mirroring the
/// local-runner 500 ms pre-flight.
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_millis(500);

/// Which embedding backend the daemon should resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbedderBackend {
    /// The bundled local semantic model (all-MiniLM / bge-small, 384-dim) —
    /// the default. Downloaded on first use into a cache; falls back to a
    /// *degraded* FNV backend if it cannot be fetched or loaded.
    #[default]
    Local,
    /// The FNV-1a bag-of-words backend (lexical, offline, always available).
    /// Select this explicitly for a no-download, keyword-only setup.
    Fnv,
    /// A learned `/v1/embeddings` backend selected by `endpoint`/`model`/`dim`.
    Learned,
}

/// Resolved `[embedder]` configuration.
///
/// Defaults to the bundled local semantic backend, so an absent block gives
/// out-of-the-box semantic recall (downloaded on first use), degrading to FNV
/// only if the model cannot be fetched or loaded.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct EmbedderConfig {
    /// Selected backend (`local` | `fnv` | `learned`).
    pub backend: EmbedderBackend,
    /// Learned endpoint base URL (e.g. `http://127.0.0.1:9090`).
    pub endpoint: Option<String>,
    /// Model name. For `local`, the bundled model (default `all-minilm-l6-v2`);
    /// for `learned`, the model id sent in `/v1/embeddings` requests. Tagged on
    /// every produced row.
    pub model: Option<String>,
    /// Vector dimension the learned model produces. Ignored for `local`, whose
    /// dimension is fixed by the chosen bundled model.
    pub dim: Option<usize>,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            backend: EmbedderBackend::Local,
            endpoint: None,
            model: None,
            dim: None,
        }
    }
}

impl EmbedderConfig {
    /// Builds the [`LocalModelSpec`](crate::local_embedder::LocalModelSpec) for
    /// the `local` backend: the configured (or default) model name plus the
    /// on-disk cache directory weights are downloaded into.
    #[must_use]
    pub fn local_spec(&self) -> crate::local_embedder::LocalModelSpec {
        crate::local_embedder::LocalModelSpec {
            model: self
                .model
                .clone()
                .unwrap_or_else(|| crate::local_embedder::DEFAULT_LOCAL_MODEL.to_owned()),
            cache_dir: crate::local_embedder::default_model_cache_dir(),
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
/// - `backend = "local"` (the default) resolves the bundled semantic model,
///   downloading it on first use; on any load failure it falls back to a
///   *degraded* [`LocalFallbackEmbedder`] (lexical FNV, flagged so the
///   degradation is visible).
/// - `backend = "fnv"` resolves the plain [`FnvEmbedder`] (intentional lexical).
/// - `backend = "learned"` resolves the [`LearnedEmbedder`] only when its
///   endpoint passes a startup health check; otherwise it falls back to the FNV
///   default. This never blocks startup and never errors.
pub async fn resolve_embedder(config: &EmbedderConfig) -> Arc<dyn Embedder> {
    match config.backend {
        EmbedderBackend::Local => resolve_local(config).await,
        EmbedderBackend::Fnv => Arc::new(FnvEmbedder::new()),
        EmbedderBackend::Learned => resolve_learned(config).await,
    }
}

/// Resolves the bundled local semantic model, degrading to a flagged FNV
/// fallback if it cannot be fetched or loaded. Never errors, never blocks
/// startup beyond the (bounded) load attempt.
async fn resolve_local(config: &EmbedderConfig) -> Arc<dyn Embedder> {
    let spec = config.local_spec();
    let model = spec.model.clone();
    let cache = spec.cache_dir.clone();
    match crate::local_embedder::load(spec).await {
        Ok(embedder) => {
            tracing::info!(
                model = %model,
                dim = embedder.dim(),
                cache = %cache.display(),
                "local semantic embedder resolved"
            );
            embedder
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                model = %model,
                "local semantic embedder unavailable (offline, uncached, or feature-disabled); vault recall DEGRADED to lexical FNV until the model can be fetched"
            );
            Arc::new(LocalFallbackEmbedder::new())
        }
    }
}

/// Resolves the learned `/v1/embeddings` backend behind a startup health check,
/// falling back to the FNV default when its endpoint is unreachable or its
/// config is incomplete.
async fn resolve_learned(config: &EmbedderConfig) -> Arc<dyn Embedder> {
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
    fn missing_config_resolves_to_local_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_embedder_config(dir.path());
        assert_eq!(
            cfg.backend,
            EmbedderBackend::Local,
            "the default backend is the bundled local semantic model"
        );
    }

    #[test]
    fn unparseable_config_resolves_to_local_default() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "[embedder\nbackend = ");
        let cfg = load_embedder_config(dir.path());
        assert_eq!(cfg.backend, EmbedderBackend::Local);
    }

    #[test]
    fn fnv_backend_block_is_read() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "[embedder]\nbackend = \"fnv\"\n");
        let cfg = load_embedder_config(dir.path());
        assert_eq!(cfg.backend, EmbedderBackend::Fnv);
    }

    #[test]
    fn local_backend_block_is_read() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            "[embedder]\nbackend = \"local\"\nmodel = \"bge-small-en-v1.5\"\n",
        );
        let cfg = load_embedder_config(dir.path());
        assert_eq!(cfg.backend, EmbedderBackend::Local);
        assert_eq!(cfg.model.as_deref(), Some("bge-small-en-v1.5"));
    }

    #[test]
    fn local_spec_defaults_model_and_targets_smedja_cache() {
        let cfg = EmbedderConfig::default();
        let spec = cfg.local_spec();
        assert_eq!(spec.model, "all-minilm-l6-v2");
        assert!(spec.cache_dir.ends_with("smedja/models"));
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
        let cfg = EmbedderConfig {
            backend: EmbedderBackend::Fnv,
            ..EmbedderConfig::default()
        };
        let e = resolve_embedder(&cfg).await;
        assert_eq!(e.model_id(), FNV_MODEL_ID);
        assert_eq!(e.dim(), 128);
        assert!(!e.status().semantic);
        assert!(
            !e.status().degraded,
            "explicit FNV is intentional, not a degradation"
        );
    }

    #[tokio::test]
    async fn local_backend_with_unloadable_model_resolves_to_degraded_fnv() {
        // An unknown model name fails to load *without touching the network*, so
        // this exercises the exact offline/load-failure fallback path
        // deterministically: resolve must hand back a lexical FNV backend that is
        // flagged degraded (semantic=false, degraded=true), never a silent swap
        // and never a download in the test gate.
        let cfg = EmbedderConfig {
            backend: EmbedderBackend::Local,
            model: Some("definitely-not-a-real-model".to_owned()),
            ..EmbedderConfig::default()
        };
        let e = resolve_embedder(&cfg).await;
        let s = e.status();
        assert_eq!(
            e.model_id(),
            FNV_MODEL_ID,
            "an unloadable local model must resolve to the FNV fallback"
        );
        assert_eq!(e.dim(), 128);
        assert!(!s.semantic, "fallback recall is lexical");
        assert!(
            s.degraded,
            "the local-model load failure must be surfaced as degraded, not silent"
        );
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
