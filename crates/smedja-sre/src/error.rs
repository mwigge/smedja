/// Errors returned by `smedja-sre` query functions.
#[derive(Debug, thiserror::Error)]
pub enum SreError {
    /// A required environment variable is absent or invalid.
    #[error("missing env var {var}: {source}")]
    MissingEnvVar {
        var: &'static str,
        source: std::env::VarError,
    },

    /// An HTTP transport or serialisation error from `reqwest`.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// The remote API returned a non-2xx status code.
    #[error("API error {status}: {body}")]
    ApiError { status: u16, body: String },
}
