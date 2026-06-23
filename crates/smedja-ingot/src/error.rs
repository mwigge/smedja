/// Errors returned by `smedja-ingot` operations.
#[derive(Debug, thiserror::Error)]
pub enum IngotError {
    /// A `rusqlite` database operation failed.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// A JSON serialisation or deserialisation failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// The schema version table contains an unexpected value.
    #[error("schema version mismatch: expected {expected}, found {found}")]
    SchemaMismatch { expected: i64, found: i64 },

    /// A blocking database task panicked (e.g. inside `spawn_blocking`) or the
    /// shared connection lock was poisoned by a prior panic.
    #[error("database task panicked: {0}")]
    TaskPanic(String),
}
