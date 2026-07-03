//! Core token and error types shared across the MCP OAuth PKCE flow.

use serde::{Deserialize, Serialize};

/// An OAuth bearer token with optional refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub access_token: String,
    pub token_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
}

/// Errors from the PKCE flow.
#[derive(Debug)]
pub enum PkceError {
    /// Network or HTTP error during token exchange.
    Http(String),
    /// Token storage or load failure.
    Storage(String),
    /// Flow cancelled or timed out.
    Cancelled,
}

impl std::fmt::Display for PkceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(e) => write!(f, "HTTP error: {e}"),
            Self::Storage(e) => write!(f, "storage error: {e}"),
            Self::Cancelled => write!(f, "OAuth flow cancelled"),
        }
    }
}

impl std::error::Error for PkceError {}
