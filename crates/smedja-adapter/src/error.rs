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
}
