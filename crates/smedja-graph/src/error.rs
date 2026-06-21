/// Errors returned by `smedja-graph` operations.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// A `rusqlite` database operation failed.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// A filesystem I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// `tree-sitter` could not parse the given source file.
    #[error("tree-sitter parse failed for {path}")]
    ParseFailed {
        /// The path of the file that could not be parsed.
        path: String,
    },
}
