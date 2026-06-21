//! Error type for `smedja-vault`.

/// Errors produced by vault operations.
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// A `rusqlite` database error occurred.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// A JSON serialisation or deserialisation error occurred.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// The query embedding has a different dimension than the stored embeddings.
    #[error("embedding dimension mismatch: expected {expected}, got {got}")]
    DimensionMismatch { expected: usize, got: usize },

    /// The embedder identity stored in the vault does not match the one being inserted.
    #[error("embedder mismatch: vault has {stored:?}, inserting with {incoming:?}")]
    EmbedderMismatch { stored: String, incoming: String },
}
