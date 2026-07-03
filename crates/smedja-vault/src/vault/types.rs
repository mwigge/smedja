//! Public data types stored in and returned by the [`Vault`](super::Vault).

/// Default model identifier assumed for rows that predate per-row model tagging.
///
/// Legacy rows were all produced by the FNV-1a bag-of-words embedder, so a row
/// whose `embedder_model_id` column is absent or NULL reads back as this id.
pub const LEGACY_MODEL_ID: &str = "fnv-bow-128";

/// A single entry stored in the vault.
#[derive(Debug, Clone, PartialEq)]
pub struct VaultEntry {
    /// Unique identifier for the entry.
    pub id: String,
    /// Embedding vector stored as raw `f32` components.
    pub embedding: Vec<f32>,
    /// Arbitrary JSON payload associated with the entry.
    pub payload: serde_json::Value,
    /// Namespace grouping for the entry. Defaults to `"default"` when empty.
    pub namespace: String,
    /// Raw text content used for keyword boosting and deduplication.
    pub content: String,
    /// Optional path to the source file that produced this entry.
    pub source_file: Option<String>,
    /// Optional identifier of the agent or process that inserted this entry.
    pub added_by: Option<String>,
    /// Position of this chunk within its parent document.
    pub chunk_index: Option<i64>,
    /// Identifier of the parent entry when this entry is a chunk.
    pub parent_id: Option<String>,
    /// Unix timestamp (seconds since epoch) when the entry was created.
    ///
    /// Set to `0.0` on construction; [`Vault::insert`](super::Vault::insert)
    /// fills in the current wall-clock time when the stored value is `0.0`.
    pub created_at: f64,
    /// Identifier of the embedding model that produced [`VaultEntry::embedding`].
    ///
    /// Persisted alongside the embedding so [`Vault::search`](super::Vault::search)/
    /// [`Vault::query`](super::Vault::query) compare only same-model vectors.
    /// Legacy rows lacking this column read back as [`LEGACY_MODEL_ID`].
    pub embedder_model_id: String,
    /// Dimension of [`VaultEntry::embedding`] as reported by its producing model.
    ///
    /// Legacy rows lacking this column derive it from the stored BLOB length
    /// divided by four (the byte width of an `f32`).
    pub dim: usize,
}

/// A single result returned by [`Vault::query`](super::Vault::query).
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// Identifier matching a [`VaultEntry::id`].
    pub id: String,
    /// Cosine similarity score in `[0.0, 1.0]`.
    pub score: f32,
    /// The payload from the matching [`VaultEntry`].
    pub payload: serde_json::Value,
}
