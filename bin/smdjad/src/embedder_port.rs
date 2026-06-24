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

/// Port for producing embedding vectors for vault text.
///
/// Implementors expose a deterministic, offline [`embed`](Embedder::embed) core,
/// a stable [`model_id`](Embedder::model_id), and the [`dim`](Embedder::dim) of
/// the vectors they produce. The async [`embed_query`](Embedder::embed_query)
/// drives the live (possibly networked) path and MUST NOT panic or abort on
/// backend failure — it degrades to a usable vector instead.
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
        }
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
            vec
        } else {
            tracing::debug!("learned endpoint unavailable; using FNV fallback for this embed");
            self.fallback.embed(text)
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
