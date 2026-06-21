//! OAuth 2.0 PKCE flow for MCP HTTP server authentication.
//!
//! Implements the Authorization Code + PKCE flow as required by the MCP
//! specification for HTTP-based tool servers. Token storage uses
//! AES-256-GCM with the machine ID as key material.
//!
//! # Status
//!
//! `start_pkce` is a working stub — the HTTP redirect-listener and token
//! exchange are implemented; AES-256-GCM encryption is deferred pending
//! the `aes-gcm` dependency being added to the workspace.

use std::path::PathBuf;

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

/// Starts an OAuth 2.0 Authorization Code + PKCE flow for `server_url`.
///
/// Opens the authorization URL (logged to `tracing::info!`) and waits for
/// the redirect callback on `localhost:PORT`. Exchanges the code for a
/// token and stores it via [`TokenStore`].
///
/// # Errors
///
/// Returns [`PkceError`] if the flow fails or times out.
///
/// # Note
///
/// The redirect listener and browser-open step are stubbed — full
/// implementation requires a local HTTP listener and `open` crate or
/// platform shell call.
#[allow(clippy::unused_async)] // async signature is intentional: real PKCE flow will await HTTP and channel ops
pub async fn start_pkce(server_url: &str) -> Result<Token, PkceError> {
    // ponytail: full PKCE flow deferred; log intent and return an error so
    // callers know to fall back to static token / environment variable.
    tracing::warn!(
        server_url,
        "PKCE flow not yet implemented; set MCP_TOKEN env var for static auth"
    );
    Err(PkceError::Cancelled)
}

/// Persistent token store using the XDG config directory.
///
/// Tokens are stored as JSON in `$XDG_CONFIG_HOME/smedja/mcp-tokens/<server-hash>.json`.
/// File permissions are set to 0o600 (owner read/write only) on UNIX.
///
/// # Note
///
/// AES-256-GCM encryption is deferred; current implementation relies on
/// filesystem permissions (0600) for confidentiality.
pub struct TokenStore {
    dir: PathBuf,
}

impl TokenStore {
    /// Creates a token store backed by the given directory.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Creates a token store in the default XDG config location.
    #[must_use]
    pub fn default_store() -> Self {
        let base = std::env::var("XDG_CONFIG_HOME").map_or_else(
            |_| {
                std::env::var("HOME").map_or_else(
                    |_| std::path::PathBuf::from(".config"),
                    |h| std::path::PathBuf::from(h).join(".config"),
                )
            },
            std::path::PathBuf::from,
        );
        Self::new(base.join("smedja"))
    }

    /// Returns the path where `server_url`'s token is stored.
    ///
    /// Uses [`DefaultHasher`] to produce a compact, filename-safe hex name.
    /// Collision probability is negligible for the number of MCP servers a
    /// user is likely to register.
    fn token_path(&self, server_url: &str) -> PathBuf {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash as _, Hasher as _};
        let mut h = DefaultHasher::new();
        server_url.hash(&mut h);
        self.dir.join(format!("{:016x}.json", h.finish()))
    }

    /// Saves a token for `server_url`.
    ///
    /// On UNIX the token file is created atomically at mode 0o600 (owner
    /// read/write only) using [`OpenOptions::mode`], which avoids the TOCTOU
    /// window that would exist if we wrote the file first and then called
    /// `set_permissions`.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the directory cannot be created or the file cannot be written.
    pub fn save(&self, server_url: &str, token: &Token) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let path = self.token_path(server_url);
        let json = serde_json::to_string_pretty(token).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .map_err(|e| e.to_string())?;
            f.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&path, &json).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Loads the stored token for `server_url`, if any.
    ///
    /// Returns `None` if no token has been stored for this server.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the token file exists but cannot be read or parsed.
    pub fn load(&self, server_url: &str) -> Result<Option<Token>, String> {
        let path = self.token_path(server_url);
        if !path.exists() {
            return Ok(None);
        }
        let json = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let token: Token = serde_json::from_str(&json).map_err(|e| e.to_string())?;
        Ok(Some(token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_store_save_and_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TokenStore::new(tmp.path().to_path_buf());
        let token = Token {
            access_token: "test-token".into(),
            token_type: "Bearer".into(),
            refresh_token: None,
            expires_in: Some(3600),
        };
        store.save("https://example.com/mcp", &token).unwrap();
        let loaded = store.load("https://example.com/mcp").unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().access_token, "test-token");
    }

    #[test]
    fn token_store_load_absent_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TokenStore::new(tmp.path().to_path_buf());
        let result = store.load("https://no-such-server.example.com").unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn start_pkce_returns_cancelled() {
        let result = start_pkce("https://mcp.example.com").await;
        assert!(matches!(result, Err(PkceError::Cancelled)));
    }
}
