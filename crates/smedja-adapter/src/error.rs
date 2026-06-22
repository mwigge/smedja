//! Error types for the `smedja-adapter` crate.

/// Errors produced by provider adapters.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// An HTTP-level error occurred while communicating with the provider.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// The response bytes could not be parsed as expected.
    #[error("parse error: {0}")]
    Parse(String),

    /// The provider returned a structurally valid response but the content was
    /// unexpected (e.g. missing required fields).
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// A request could not be dispatched (e.g. the subprocess binary was not
    /// found or could not be spawned).
    #[error("request error: {0}")]
    Request(String),

    /// The provider returned HTTP 429 Too Many Requests.
    ///
    /// `retry_after` is parsed from the `Retry-After` response header when
    /// present.  Callers should back off for at least this duration before
    /// retrying.
    #[error("rate limited by provider (retry after {retry_after:?})")]
    RateLimited {
        /// Suggested back-off duration from the provider, if supplied.
        retry_after: Option<std::time::Duration>,
    },
}
