//! `Embedder` port — the daemon's single swap point for vault-bound embeddings.
//!
//! Mirrors the [`smedja_memory::ColdStore`] port: a trait the daemon owns, with
//! concrete backends ([`FnvEmbedder`], [`LearnedEmbedder`]) supplied at startup.
//! Every vault-embedding call site invokes the resolved `Arc<dyn Embedder>`
//! rather than the free `crate::embedder::embed` function, so the backend choice
//! and the `model_id`/`dim` pairing live in one place.
//!
//! The synchronous [`Embedder::embed`] is the offline core (FNV bag-of-words);
//! the async [`Embedder::embed_query`] is the live path a learned backend uses
//! for network I/O, degrading to the synchronous core on any failure so a
//! missing or unreachable model never hard-fails a turn.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;

/// Stable identifier reported by the FNV-1a bag-of-words backend.
pub const FNV_MODEL_ID: &str = "fnv-bow-128";

/// Request timeout for a live `/v1/embeddings` call.
///
/// A slow or unreachable endpoint must degrade to the FNV fallback rather than
/// stall the turn, so the live call is bounded.
const EMBED_TIMEOUT: Duration = Duration::from_secs(5);

/// Observable recall-quality state of the resolved [`Embedder`].
///
/// This is the "no silent degrade" surface: a caller (and the operator, via a
/// startup log) can see whether vault recall is genuinely *semantic* or has
/// fallen back to lexical FNV keyword overlap. Without this, a learned endpoint
/// going down would quietly turn semantic recall into keyword search with no
/// signal anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedderStatus {
    /// Model id tagged on rows this backend produces.
    pub model_id: String,
    /// Vector dimension produced.
    pub dim: usize,
    /// `true` when the backend is a real semantic model; `false` for the lexical
    /// FNV bag-of-words backend.
    pub semantic: bool,
    /// `true` when a semantic backend is currently serving the FNV fallback
    /// because its endpoint is unreachable — recall is lexical *right now*
    /// despite `semantic == true`. Always `false` for the FNV backend, whose FNV
    /// output is intentional rather than a degradation.
    pub degraded: bool,
    /// Count of live embeds that have fallen back to FNV over this process's
    /// lifetime. Non-zero on a semantic backend means recall has been lexical for
    /// at least some queries.
    pub fallback_count: u64,
}

/// Port for producing embedding vectors for vault text.
///
/// Implementors expose a deterministic, offline [`embed`](Embedder::embed) core,
/// a stable [`model_id`](Embedder::model_id), and the [`dim`](Embedder::dim) of
/// the vectors they produce. The async [`embed_query`](Embedder::embed_query)
/// drives the live (possibly networked) path and MUST NOT panic or abort on
/// backend failure — it degrades to a usable vector instead, and records that
/// degradation on [`status`](Embedder::status) rather than swallowing it.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embeds `text` synchronously into a [`dim`](Embedder::dim)-length vector.
    ///
    /// This is the offline core. For a networked backend it is the local
    /// fallback (FNV), never the network call — synchronous code (e.g. inside
    /// [`tokio::task::spawn_blocking`]) cannot await the endpoint.
    fn embed(&self, text: &str) -> Vec<f32>;

    /// Returns the stable model identifier persisted on every row this embedder
    /// produces (e.g. `"fnv-bow-128"`).
    fn model_id(&self) -> &str;

    /// Returns the dimension of the vectors this embedder produces.
    fn dim(&self) -> usize;

    /// Embeds `text` on the live path, using network I/O where the backend
    /// supports it.
    ///
    /// The default delegates to [`embed`](Embedder::embed); a learned backend
    /// overrides it to call its endpoint and fall back to [`embed`](Embedder::embed)
    /// on any transport error, non-success status, or timeout. It never panics
    /// or returns an error — a missing model degrades recall, it does not abort
    /// the turn.
    async fn embed_query(&self, text: &str) -> Vec<f32> {
        self.embed(text)
    }

    /// Returns the backend's current recall-quality [`EmbedderStatus`].
    ///
    /// The default reports a non-semantic, non-degraded backend (the FNV core):
    /// its FNV output is the intended behaviour, not a fallback. A semantic
    /// backend overrides this to report `semantic = true` and whether it is
    /// currently [`degraded`](EmbedderStatus::degraded) to the FNV fallback.
    fn status(&self) -> EmbedderStatus {
        EmbedderStatus {
            model_id: self.model_id().to_owned(),
            dim: self.dim(),
            semantic: false,
            degraded: false,
            fallback_count: 0,
        }
    }
}

/// FNV-1a bag-of-words backend — the named offline default.
///
/// Wraps the existing [`crate::embedder::embed`] / [`crate::embedder::DIM`] so
/// the default backend's output stays byte-identical to the historical embedder.
/// Always available; selected whenever no learned backend is configured or
/// reachable.
#[derive(Debug, Clone, Default)]
pub struct FnvEmbedder;

impl FnvEmbedder {
    /// Creates the FNV-1a backend.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Embedder for FnvEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        crate::embedder::embed(text)
    }

    fn model_id(&self) -> &str {
        FNV_MODEL_ID
    }

    fn dim(&self) -> usize {
        crate::embedder::DIM
    }
}

/// Learned backend — a local OpenAI-compatible `/v1/embeddings` client.
///
/// Issues `POST {endpoint}/v1/embeddings` with `{ "model", "input" }` and uses
/// `data[0].embedding`, reusing the local-runner HTTP/timeout shape. The
/// configured `model_id`/`dim` tag every row this backend produces.
///
/// Per the never-hard-fail contract, any transport error, non-success status,
/// timeout, or malformed body on the live [`embed_query`](Embedder::embed_query)
/// path falls back to the FNV vector — the turn is never aborted. The
/// synchronous [`embed`](Embedder::embed) is always the FNV fallback because it
/// cannot await the endpoint.
pub struct LearnedEmbedder {
    client: Client,
    /// Base endpoint serving the OpenAI-compatible API.
    endpoint: String,
    /// Model id sent in the request body and tagged on produced rows.
    model_id: String,
    /// Vector dimension the configured model produces.
    dim: usize,
    /// Offline fallback used by `embed` and whenever the endpoint fails.
    fallback: FnvEmbedder,
    /// `true` while the endpoint is unreachable and live embeds are serving the
    /// FNV fallback. Flipped on the first failure and cleared on recovery so the
    /// degrade/recover transitions are logged exactly once each, not per query.
    degraded: AtomicBool,
    /// Total live embeds that have fallen back to FNV over this process lifetime.
    fallback_count: AtomicU64,
}

impl LearnedEmbedder {
    /// Creates a learned backend targeting `endpoint` with the configured
    /// `model_id` and `dim`.
    #[must_use]
    pub fn new(endpoint: impl Into<String>, model_id: impl Into<String>, dim: usize) -> Self {
        let client = Client::builder()
            .timeout(EMBED_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            client,
            endpoint: endpoint.into(),
            model_id: model_id.into(),
            dim,
            fallback: FnvEmbedder::new(),
            degraded: AtomicBool::new(false),
            fallback_count: AtomicU64::new(0),
        }
    }

    /// Records a live fallback to FNV and surfaces the *transition* into a
    /// degraded state exactly once (a per-query warning would flood the log while
    /// the endpoint stays down). Recall is lexical until [`note_success`] clears
    /// the flag.
    ///
    /// [`note_success`]: LearnedEmbedder::note_success
    fn note_fallback(&self) {
        self.fallback_count.fetch_add(1, Ordering::Relaxed);
        if !self.degraded.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                model = %self.model_id,
                "embedder DEGRADED: learned endpoint unreachable, vault recall has fallen back to lexical FNV keyword overlap until it recovers"
            );
        } else {
            tracing::debug!(model = %self.model_id, "learned endpoint still degraded; FNV fallback");
        }
    }

    /// Clears the degraded flag on a successful live embed, logging the recovery
    /// once (only when transitioning out of a degraded episode).
    fn note_success(&self) {
        if self.degraded.swap(false, Ordering::Relaxed) {
            tracing::info!(
                model = %self.model_id,
                "embedder recovered: learned endpoint reachable again, semantic recall restored"
            );
        }
    }

    /// Current fallback count — the number of live embeds served by FNV.
    #[must_use]
    pub fn fallback_count(&self) -> u64 {
        self.fallback_count.load(Ordering::Relaxed)
    }

    /// Requests an embedding from the endpoint, returning `None` on any failure.
    ///
    /// A `None` result is the signal to fall back to FNV; it is never surfaced
    /// as an error to the caller.
    async fn request_embedding(&self, text: &str) -> Option<Vec<f32>> {
        let url = format!("{}/v1/embeddings", self.endpoint.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "model": self.model_id, "input": text }))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            tracing::debug!(status = %resp.status(), "learned embed non-success; falling back to FNV");
            return None;
        }
        let body = resp.json::<serde_json::Value>().await.ok()?;
        parse_embedding(&body)
    }
}

#[async_trait]
impl Embedder for LearnedEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        // Synchronous core cannot reach the endpoint; degrade to FNV.
        self.fallback.embed(text)
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_query(&self, text: &str) -> Vec<f32> {
        if let Some(vec) = self.request_embedding(text).await {
            self.note_success();
            vec
        } else {
            // No longer a silent debug line: fallback is tracked and the
            // degrade transition is surfaced at WARN via `note_fallback`.
            self.note_fallback();
            self.fallback.embed(text)
        }
    }

    fn status(&self) -> EmbedderStatus {
        EmbedderStatus {
            model_id: self.model_id.clone(),
            dim: self.dim,
            semantic: true,
            degraded: self.degraded.load(Ordering::Relaxed),
            fallback_count: self.fallback_count.load(Ordering::Relaxed),
        }
    }
}

/// Extracts `data[0].embedding` from a `/v1/embeddings` response body.
///
/// Returns `None` when the structure is absent or an element is non-numeric, so
/// the caller falls back rather than panicking.
fn parse_embedding(body: &serde_json::Value) -> Option<Vec<f32>> {
    let arr = body
        .get("data")?
        .as_array()?
        .first()?
        .get("embedding")?
        .as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        #[allow(clippy::cast_possible_truncation)]
        // f64→f32 narrowing is acceptable for embeddings
        out.push(v.as_f64()? as f32);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_embedder_reports_stable_identity() {
        let e = FnvEmbedder::new();
        assert_eq!(e.model_id(), "fnv-bow-128");
        assert_eq!(e.dim(), 128);
    }

    #[test]
    fn fnv_embedder_embed_has_dim_length() {
        let e = FnvEmbedder::new();
        assert_eq!(e.embed("hello world").len(), 128);
    }

    #[test]
    fn fnv_embedder_is_byte_identical_to_free_function() {
        let e = FnvEmbedder::new();
        for text in ["hello world", "rust async tokio", "", "Mixed CASE Words"] {
            assert_eq!(
                e.embed(text),
                crate::embedder::embed(text),
                "FnvEmbedder::embed must match the historical embedder::embed for {text:?}",
            );
        }
    }

    #[tokio::test]
    async fn fnv_embed_query_delegates_to_embed() {
        let e = FnvEmbedder::new();
        let query = "renew the session credential";
        assert_eq!(e.embed_query(query).await, e.embed(query));
    }

    #[test]
    fn fnv_embedder_is_usable_as_trait_object() {
        let e: std::sync::Arc<dyn Embedder> = std::sync::Arc::new(FnvEmbedder::new());
        assert_eq!(e.model_id(), "fnv-bow-128");
        assert_eq!(e.dim(), 128);
        assert_eq!(e.embed("x").len(), 128);
    }

    // ── learned backend ───────────────────────────────────────────────────────

    #[test]
    fn parse_embedding_extracts_first_vector() {
        let body = serde_json::json!({
            "data": [{ "embedding": [0.1, 0.2, 0.3] }]
        });
        assert_eq!(parse_embedding(&body), Some(vec![0.1_f32, 0.2, 0.3]));
    }

    #[test]
    fn parse_embedding_none_on_malformed_body() {
        assert!(parse_embedding(&serde_json::json!({})).is_none());
        assert!(parse_embedding(&serde_json::json!({ "data": [] })).is_none());
    }

    #[tokio::test]
    async fn learned_embedder_uses_endpoint_vector_when_available() {
        let server = MockEmbeddingsServer::spawn(200, vec![0.5, 0.5, 0.5, 0.5]).await;
        let e = LearnedEmbedder::new(server.base_url(), "minilm-l6-v2", 4);
        assert_eq!(e.model_id(), "minilm-l6-v2");
        assert_eq!(e.dim(), 4);
        let v = e.embed_query("auth token refresh").await;
        assert_eq!(v, vec![0.5_f32, 0.5, 0.5, 0.5]);
    }

    #[tokio::test]
    async fn learned_embedder_falls_back_to_fnv_when_unreachable() {
        // Port 1 refuses connections immediately — the live path must not panic.
        let e = LearnedEmbedder::new("http://127.0.0.1:1", "minilm-l6-v2", 4);
        let v = e.embed_query("auth token refresh").await;
        // Falls back to the FNV vector (DIM=128), never an empty/panic.
        assert_eq!(v, crate::embedder::embed("auth token refresh"));
    }

    #[tokio::test]
    async fn learned_embedder_falls_back_on_non_success_status() {
        let server = MockEmbeddingsServer::spawn(500, vec![]).await;
        let e = LearnedEmbedder::new(server.base_url(), "minilm-l6-v2", 4);
        let v = e.embed_query("hello").await;
        assert_eq!(v, crate::embedder::embed("hello"));
    }

    // ── status / no silent degrade ────────────────────────────────────────────

    #[test]
    fn fnv_status_is_semantic_false_and_never_degraded() {
        let s = FnvEmbedder::new().status();
        assert!(!s.semantic, "FNV is a lexical backend");
        assert!(!s.degraded, "FNV output is intended, never a degradation");
        assert_eq!(s.fallback_count, 0);
        assert_eq!(s.model_id, FNV_MODEL_ID);
        assert_eq!(s.dim, 128);
    }

    #[tokio::test]
    async fn learned_status_reports_healthy_before_any_failure() {
        let e = LearnedEmbedder::new("http://127.0.0.1:9", "minilm-l6-v2", 4);
        let s = e.status();
        assert!(s.semantic, "a learned backend is semantic");
        assert!(!s.degraded, "no live embed has failed yet");
        assert_eq!(s.fallback_count, 0);
    }

    #[tokio::test]
    async fn learned_fallback_is_surfaced_not_silent() {
        // Unreachable endpoint → the live embed degrades, and that degradation is
        // observable on the status surface (a status field + counter), never
        // silently swallowed.
        let e = LearnedEmbedder::new("http://127.0.0.1:1", "minilm-l6-v2", 4);
        assert!(!e.status().degraded, "starts healthy");

        let _ = e.embed_query("auth token refresh").await;
        let s = e.status();
        assert!(
            s.degraded,
            "after a failed live embed the backend must report degraded"
        );
        assert_eq!(s.fallback_count, 1, "the fallback must be counted");
        assert!(
            s.semantic,
            "still a semantic backend, just degraded right now"
        );

        // A second failure keeps it degraded and increments the counter.
        let _ = e.embed_query("second query").await;
        assert_eq!(e.fallback_count(), 2);
        assert!(e.status().degraded);
    }

    #[tokio::test]
    async fn learned_status_clears_after_recovery() {
        // Degrade against a dead port, then point a fresh call at a live server
        // by constructing a new embedder — here we assert the recovery path clears
        // the flag when a live embed succeeds.
        let server = MockEmbeddingsServer::spawn(200, vec![0.1, 0.2, 0.3, 0.4]).await;
        let e = LearnedEmbedder::new(server.base_url(), "minilm-l6-v2", 4);
        // Force a degraded state first via the internal transition.
        e.note_fallback();
        assert!(e.status().degraded);
        // A successful live embed must clear the degraded flag (note_success).
        let v = e.embed_query("hello").await;
        assert_eq!(v, vec![0.1_f32, 0.2, 0.3, 0.4]);
        assert!(
            !e.status().degraded,
            "a successful live embed must clear the degraded flag"
        );
        // The historical fallback is still counted for observability.
        assert_eq!(e.status().fallback_count, 1);
    }

    /// Minimal one-shot HTTP server returning a fixed `/v1/embeddings` response.
    struct MockEmbeddingsServer {
        port: u16,
    }

    impl MockEmbeddingsServer {
        async fn spawn(status: u16, embedding: Vec<f32>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind ephemeral port");
            let port = listener.local_addr().expect("local addr").port();
            tokio::spawn(async move {
                if let Ok((mut stream, _)) = listener.accept().await {
                    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf).await;
                    let reason = if status == 200 { "OK" } else { "Error" };
                    let body = serde_json::json!({
                        "data": [{ "embedding": embedding }]
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.flush().await;
                }
            });
            Self { port }
        }

        fn base_url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }
    }
}
