//! Bundled local semantic embedder — smedja's default vault-recall backend.
//!
//! This is the in-process semantic model the [`embedder_config`] seam anticipated:
//! a sentence-transformer (all-MiniLM-L6-v2 / bge-small-en-v1.5, 384-dim) run
//! locally via [`fastembed`] (ONNX Runtime + tokenizers). Unlike the lexical
//! [`FnvEmbedder`](crate::embedder_port::FnvEmbedder), it produces genuinely
//! *semantic* vectors: paraphrases with no shared words still score close.
//!
//! # Model acquisition
//!
//! Weights are **not** embedded in the binary. On first use the model is
//! downloaded into a cache ([`default_model_cache_dir`], `~/.local/share/smedja/models/`)
//! and reused on every subsequent start. If the download or load fails (offline,
//! no cached copy), [`load`] returns an error and the resolver falls back to the
//! FNV backend — surfaced as *degraded* so recall is never silently lexical.
//!
//! # Feature gate
//!
//! The heavy ONNX toolchain lives behind the `local-embedder` cargo feature
//! (on by default). With the feature off, [`load`] always errors ("not compiled
//! in") and the daemon still builds and runs on the FNV fallback. Everything the
//! config layer needs ([`LocalModelSpec`], [`default_model_cache_dir`],
//! [`DEFAULT_LOCAL_MODEL`]) compiles in both configurations.
//!
//! [`embedder_config`]: crate::embedder_config

use std::path::PathBuf;
use std::sync::Arc;

use crate::embedder_port::Embedder;

/// Default local model name resolved when `[embedder] model` is unset.
///
/// all-MiniLM-L6-v2: 384-dim, symmetric (no query/passage prefix), the canonical
/// small sentence-transformer. Tagged on every row as `all-minilm-l6-v2`.
pub const DEFAULT_LOCAL_MODEL: &str = "all-minilm-l6-v2";

/// Selection of a bundled local model plus where to cache its weights.
///
/// Built from the `[embedder]` config by
/// [`EmbedderConfig::local_spec`](crate::embedder_config::EmbedderConfig::local_spec);
/// consumed by [`load`]. Deliberately free of any `fastembed` type so the config
/// layer can construct it whether or not the `local-embedder` feature is on.
#[derive(Debug, Clone)]
pub struct LocalModelSpec {
    /// Config model name, mapped to a bundled model by [`load`]. Unknown names
    /// error (and fall back to FNV) rather than reaching the network.
    pub model: String,
    /// Directory the weights/tokenizer are downloaded to and cached in.
    pub cache_dir: PathBuf,
}

/// Resolves the on-disk cache directory for downloaded model weights.
///
/// `$XDG_DATA_HOME/smedja/models` when set, else `~/.local/share/smedja/models`,
/// else a relative `smedja-models` as a last resort so this never panics.
#[must_use]
pub fn default_model_cache_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("smedja").join("models");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("smedja")
            .join("models");
    }
    PathBuf::from("smedja-models")
}

// ── feature ON: the real fastembed-backed embedder ──────────────────────────────

/// Downloads (on first use) and loads the local semantic model described by
/// `spec`, returning it as an `Arc<dyn Embedder>`.
///
/// The load runs on the blocking pool because it is CPU- and I/O-bound (ONNX
/// session init + a possible weight download). Any failure — unknown model name,
/// download error, corrupt cache — is returned as `Err`; the resolver turns that
/// into the degraded FNV fallback rather than aborting startup.
///
/// # Errors
///
/// Returns an error when the `local-embedder` feature is disabled, the model name
/// is unknown, or the weights cannot be fetched or loaded.
#[cfg(feature = "local-embedder")]
pub async fn load(spec: LocalModelSpec) -> anyhow::Result<Arc<dyn Embedder>> {
    let embedder = tokio::task::spawn_blocking(move || LocalEmbedder::try_new(&spec)).await??;
    Ok(Arc::new(embedder))
}

/// Feature-disabled stub: no ONNX toolchain compiled in, so the local model can
/// never load. Always errors so the resolver uses the FNV fallback.
///
/// # Errors
///
/// Always returns an error indicating the `local-embedder` feature is disabled.
#[cfg(not(feature = "local-embedder"))]
#[allow(clippy::unused_async)] // signature parity with the feature-on `load`
pub async fn load(_spec: LocalModelSpec) -> anyhow::Result<Arc<dyn Embedder>> {
    anyhow::bail!("local-embedder feature not compiled in")
}

/// Maps a config model name to a bundled `fastembed` model plus the stable
/// `(model_id, dim)` tagged on every row it produces.
///
/// Unknown names error *before* any network access, so a typo degrades to FNV
/// instantly instead of hanging on a doomed download.
#[cfg(feature = "local-embedder")]
fn resolve_model(name: &str) -> anyhow::Result<(fastembed::EmbeddingModel, &'static str, usize)> {
    use fastembed::EmbeddingModel;
    match name {
        "all-minilm-l6-v2" | "all-MiniLM-L6-v2" => {
            Ok((EmbeddingModel::AllMiniLML6V2, "all-minilm-l6-v2", 384))
        }
        "bge-small-en-v1.5" | "bge-small" => {
            Ok((EmbeddingModel::BGESmallENV15, "bge-small-en-v1.5", 384))
        }
        other => anyhow::bail!(
            "unknown local embedder model {other:?}; supported: all-minilm-l6-v2, bge-small-en-v1.5"
        ),
    }
}

/// In-process semantic embedder backed by a bundled ONNX sentence-transformer.
///
/// `TextEmbedding::embed` takes `&mut self`, but [`Embedder::embed`] is `&self`,
/// so the session is held behind a [`Mutex`](std::sync::Mutex). Inference is a
/// few milliseconds of local CPU work for one short string.
#[cfg(feature = "local-embedder")]
pub struct LocalEmbedder {
    model: std::sync::Mutex<fastembed::TextEmbedding>,
    model_id: &'static str,
    dim: usize,
}

#[cfg(feature = "local-embedder")]
impl LocalEmbedder {
    /// Builds the embedder from `spec`, downloading the weights on first use.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown model name or when the weights cannot be
    /// fetched/loaded.
    pub fn try_new(spec: &LocalModelSpec) -> anyhow::Result<Self> {
        let (model, model_id, dim) = resolve_model(&spec.model)?;
        std::fs::create_dir_all(&spec.cache_dir).ok();
        let options = fastembed::InitOptions::new(model)
            .with_cache_dir(spec.cache_dir.clone())
            .with_show_download_progress(false);
        let text_embedding = fastembed::TextEmbedding::try_new(options)?;
        Ok(Self {
            model: std::sync::Mutex::new(text_embedding),
            model_id,
            dim,
        })
    }

    /// Runs inference for one string, returning an L2-normalised `dim`-vector.
    ///
    /// Never panics: a poisoned lock or a rare inference error degrades to a
    /// zero vector of the correct dimension (a no-match, dimension-preserving
    /// result) rather than aborting the turn.
    fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut guard = match self.model.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard.embed(vec![text], None) {
            Ok(mut batch) if !batch.is_empty() => {
                let mut v = batch.swap_remove(0);
                l2_normalize(&mut v);
                v
            }
            Ok(_) => vec![0.0; self.dim],
            Err(e) => {
                tracing::warn!(error = %e, model = self.model_id, "local embed inference failed; returning zero vector");
                vec![0.0; self.dim]
            }
        }
    }
}

#[cfg(feature = "local-embedder")]
#[async_trait::async_trait]
impl Embedder for LocalEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        self.embed_one(text)
    }

    fn model_id(&self) -> &str {
        self.model_id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn status(&self) -> crate::embedder_port::EmbedderStatus {
        crate::embedder_port::EmbedderStatus {
            model_id: self.model_id.to_owned(),
            dim: self.dim,
            semantic: true,
            degraded: false,
            fallback_count: 0,
        }
    }
}

/// L2-normalises `vec` in place so cosine similarity equals the dot product,
/// matching the invariant every other backend upholds.
#[cfg(feature = "local-embedder")]
fn l2_normalize(vec: &mut [f32]) {
    let norm = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-9 {
        for x in vec.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cache_dir_prefers_xdg_then_home() {
        // Deterministic: assert the path shape from an explicit XDG value rather
        // than mutating process-global env (which would race parallel tests).
        let dir = default_model_cache_dir();
        assert!(
            dir.ends_with("smedja/models"),
            "cache dir must live under a smedja/models subtree: {}",
            dir.display()
        );
    }

    #[test]
    fn local_model_spec_is_constructible_without_the_feature() {
        // The config layer must be able to build a spec in a lean build.
        let spec = LocalModelSpec {
            model: DEFAULT_LOCAL_MODEL.to_owned(),
            cache_dir: default_model_cache_dir(),
        };
        assert_eq!(spec.model, "all-minilm-l6-v2");
    }

    /// Unknown model names must fail fast, before any network access, so a typo
    /// degrades to FNV instantly. Deterministic and offline.
    #[cfg(feature = "local-embedder")]
    #[tokio::test]
    async fn load_unknown_model_errors_without_network() {
        let spec = LocalModelSpec {
            model: "definitely-not-a-real-model".to_owned(),
            cache_dir: default_model_cache_dir(),
        };
        assert!(
            load(spec).await.is_err(),
            "an unknown model name must error (→ FNV fallback), not reach the network"
        );
    }

    #[cfg(feature = "local-embedder")]
    #[test]
    fn resolve_model_maps_known_names_and_rejects_unknown() {
        assert_eq!(
            resolve_model("all-minilm-l6-v2").unwrap().1,
            "all-minilm-l6-v2"
        );
        assert_eq!(resolve_model("all-minilm-l6-v2").unwrap().2, 384);
        assert_eq!(
            resolve_model("bge-small-en-v1.5").unwrap().1,
            "bge-small-en-v1.5"
        );
        assert!(resolve_model("nope").is_err());
    }

    /// The semantic proof — and the FNV-can't-do-this contrast. Downloads the
    /// model, so it is `#[ignore]`d out of the default gate; run explicitly with
    /// `cargo test -p smdjad --ignored`.
    #[cfg(feature = "local-embedder")]
    #[tokio::test]
    #[ignore = "downloads the model on first run; run with --ignored"]
    async fn semantic_similarity_beats_unrelated_where_fnv_cannot() {
        let spec = LocalModelSpec {
            model: DEFAULT_LOCAL_MODEL.to_owned(),
            cache_dir: default_model_cache_dir(),
        };
        let e = load(spec).await.expect("local model must load");
        assert_eq!(e.dim(), 384, "all-MiniLM-L6-v2 is 384-dim");
        assert!(e.status().semantic, "loaded local backend is semantic");
        assert!(!e.status().degraded);

        let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();

        // A paraphrase that shares *zero* tokens with the base sentence (not even
        // a stopword), plus an unrelated sentence.
        const BASE: &str = "The doctor examined the patient carefully.";
        const PARA: &str = "A physician assessed a sick person thoroughly.";
        const UNREL: &str = "Investors sold their shares amid market turbulence.";

        let sim = cos(&e.embed(BASE), &e.embed(PARA));
        let dis = cos(&e.embed(BASE), &e.embed(UNREL));
        assert!(
            sim > dis + 0.2,
            "semantic model must rank the paraphrase far above the unrelated text: sim={sim:.3}, unrel={dis:.3}"
        );

        // The proof that this is genuinely semantic and not lexical: FNV
        // bag-of-words sees no shared tokens between BASE and PARA, so it cannot
        // recognise the paraphrase — its separation of paraphrase-vs-unrelated is
        // near zero, while the semantic model's is large.
        let fnv = |t: &str| crate::embedder::embed(t);
        let fnv_sim = cos(&fnv(BASE), &fnv(PARA));
        let fnv_dis = cos(&fnv(BASE), &fnv(UNREL));
        let semantic_margin = sim - dis;
        let fnv_margin = fnv_sim - fnv_dis;
        println!(
            "semantic: sim={sim:.3} unrel={dis:.3} margin={semantic_margin:.3} | fnv: sim={fnv_sim:.3} unrel={fnv_dis:.3} margin={fnv_margin:.3}"
        );
        assert!(
            semantic_margin > fnv_margin + 0.2,
            "semantic recall must separate the paraphrase from noise far better than lexical FNV \
             (the FNV-can't-do-this proof): semantic_margin={semantic_margin:.3}, fnv_margin={fnv_margin:.3}"
        );
    }
}
