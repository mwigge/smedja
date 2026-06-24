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

    /// The provider rejected the request because its quota or credit budget is
    /// exhausted (e.g. HTTP 403 / `insufficient_quota`).
    ///
    /// Unlike [`RateLimited`](Self::RateLimited) this is not relieved by backing
    /// off against the same provider; the turn must rotate to another provider.
    #[error("provider quota exhausted: {0}")]
    QuotaExhausted(String),

    /// The provider rejected the request because the prompt exceeds the model's
    /// context window (e.g. `context_length_exceeded` / "prompt is too long").
    ///
    /// Rotating to a strictly-more-capable provider may serve the turn; retrying
    /// the same provider with the same prompt cannot.
    #[error("context length exceeded: {0}")]
    ContextLengthExceeded(String),
}

impl AdapterError {
    /// Returns `true` when the failure may be recovered by rotating the turn to
    /// another eligible provider.
    ///
    /// Rate-limit, quota-exhausted, context-length, and provider-down
    /// (transport-level / 5xx [`Http`](Self::Http)) failures are retryable.
    /// [`Parse`](Self::Parse), [`InvalidResponse`](Self::InvalidResponse), and
    /// non-transport [`Request`](Self::Request) errors are not — rotating cannot
    /// fix a malformed response or a missing-binary configuration fault.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::QuotaExhausted(_) | Self::ContextLengthExceeded(_) => {
                true
            }
            // Transport-level / connection / 5xx faults indicate the provider is
            // down or unreachable; a different provider may serve the turn.
            Self::Http(e) => is_provider_down(e),
            Self::Parse(_) | Self::InvalidResponse(_) | Self::Request(_) => false,
        }
    }

    /// Returns a stable classification string used directly as the
    /// `smedja.error.kind` telemetry attribute value.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RateLimited { .. } => "rate_limited",
            Self::QuotaExhausted(_) => "quota_exhausted",
            Self::ContextLengthExceeded(_) => "context_length_exceeded",
            Self::Http(e) => {
                if is_provider_down(e) {
                    "provider_down"
                } else {
                    "http"
                }
            }
            Self::Parse(_) => "parse",
            Self::InvalidResponse(_) => "invalid_response",
            Self::Request(_) => "request",
        }
    }
}

/// Classifies a non-success HTTP response (status other than 429, which callers
/// handle separately) into the most specific [`AdapterError`].
///
/// Quota-exhausted responses (HTTP 403 or an `insufficient_quota` marker in the
/// body) map to [`AdapterError::QuotaExhausted`]; context-window overflows
/// (`context_length_exceeded` or "prompt is too long") map to
/// [`AdapterError::ContextLengthExceeded`]; everything else — including 5xx,
/// which the adapter cannot represent as a transport-level [`Http`](AdapterError::Http)
/// error — stays [`AdapterError::InvalidResponse`]. Genuine provider-down faults
/// (connection refused, timeouts) surface as [`Http`](AdapterError::Http) from
/// `reqwest` and are classified retryable there.
#[must_use]
pub fn classify_http_error(status: reqwest::StatusCode, body: &str) -> AdapterError {
    let lower = body.to_lowercase();
    if status == reqwest::StatusCode::FORBIDDEN || lower.contains("insufficient_quota") {
        return AdapterError::QuotaExhausted(format!("HTTP {status}: {body}"));
    }
    if lower.contains("context_length_exceeded") || lower.contains("prompt is too long") {
        return AdapterError::ContextLengthExceeded(format!("HTTP {status}: {body}"));
    }
    AdapterError::InvalidResponse(format!("HTTP {status}: {body}"))
}

/// Returns `true` when a [`reqwest::Error`] indicates the provider is down or
/// unreachable (transport/connection fault or a 5xx server response) rather than
/// a well-formed client-side error.
fn is_provider_down(e: &reqwest::Error) -> bool {
    if e.is_timeout() || e.is_connect() || e.is_request() {
        return true;
    }
    e.status().is_some_and(|s| s.is_server_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limited_is_retryable_with_stable_kind() {
        let err = AdapterError::RateLimited { retry_after: None };
        assert!(err.is_retryable(), "rate_limited must be retryable");
        assert_eq!(err.kind(), "rate_limited");
    }

    #[test]
    fn quota_exhausted_is_retryable_with_stable_kind() {
        let err = AdapterError::QuotaExhausted("insufficient_quota".to_owned());
        assert!(err.is_retryable(), "quota_exhausted must be retryable");
        assert_eq!(err.kind(), "quota_exhausted");
    }

    #[test]
    fn context_length_exceeded_is_retryable_with_stable_kind() {
        let err = AdapterError::ContextLengthExceeded("prompt is too long".to_owned());
        assert!(
            err.is_retryable(),
            "context_length_exceeded must be retryable"
        );
        assert_eq!(err.kind(), "context_length_exceeded");
    }

    #[test]
    fn parse_error_is_not_retryable() {
        let err = AdapterError::Parse("bad json".to_owned());
        assert!(!err.is_retryable(), "parse must not be retryable");
        assert_eq!(err.kind(), "parse");
    }

    #[test]
    fn invalid_response_is_not_retryable() {
        let err = AdapterError::InvalidResponse("missing field".to_owned());
        assert!(
            !err.is_retryable(),
            "invalid_response must not be retryable"
        );
        assert_eq!(err.kind(), "invalid_response");
    }

    #[test]
    fn request_error_is_not_retryable() {
        let err = AdapterError::Request("binary not found".to_owned());
        assert!(!err.is_retryable(), "request error must not be retryable");
        assert_eq!(err.kind(), "request");
    }

    #[test]
    fn classify_http_403_maps_to_quota_exhausted() {
        let err = classify_http_error(reqwest::StatusCode::FORBIDDEN, "forbidden");
        assert_eq!(err.kind(), "quota_exhausted");
        assert!(err.is_retryable());
    }

    #[test]
    fn classify_insufficient_quota_body_maps_to_quota_exhausted() {
        let err = classify_http_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"code":"insufficient_quota"}}"#,
        );
        assert_eq!(err.kind(), "quota_exhausted");
    }

    #[test]
    fn classify_context_length_body_maps_to_context_length_exceeded() {
        let err = classify_http_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":{"code":"context_length_exceeded"}}"#,
        );
        assert_eq!(err.kind(), "context_length_exceeded");
        assert!(err.is_retryable());
    }

    #[test]
    fn classify_prompt_too_long_body_maps_to_context_length_exceeded() {
        let err = classify_http_error(
            reqwest::StatusCode::BAD_REQUEST,
            "prompt is too long: 250000 tokens > 200000 maximum",
        );
        assert_eq!(err.kind(), "context_length_exceeded");
    }

    #[test]
    fn classify_other_4xx_stays_invalid_response() {
        let err = classify_http_error(reqwest::StatusCode::BAD_REQUEST, "bad request");
        assert_eq!(err.kind(), "invalid_response");
        assert!(!err.is_retryable());
    }
}
