//! Persistent token store backed by the XDG config directory.

use std::path::PathBuf;

use super::token::Token;

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
    /// Uses the first 16 hex characters of a SHA-256 digest (64 bits) to
    /// produce a compact, deterministic, filename-safe name.  SHA-256 is
    /// stable across Rust versions and compilers, unlike `DefaultHasher`.
    fn token_path(&self, server_url: &str) -> PathBuf {
        use sha2::{Digest as _, Sha256};
        let hash = format!("{:x}", Sha256::digest(server_url.as_bytes()));
        self.dir.join(format!("{}.json", &hash[..16]))
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

    #[test]
    fn token_store_round_trips_access_token() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TokenStore::new(tmp.path().to_path_buf());
        let token = Token {
            access_token: "round-trip-token".into(),
            token_type: "Bearer".into(),
            refresh_token: Some("refresh".into()),
            expires_in: Some(3600),
        };
        store.save("https://mcp.example.com", &token).unwrap();
        let loaded = store
            .load("https://mcp.example.com")
            .unwrap()
            .expect("token should be present after save");
        assert_eq!(loaded.access_token, "round-trip-token");
    }

    #[test]
    fn token_path_is_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TokenStore::new(tmp.path().to_path_buf());

        let url = "https://mcp.example.com";
        let path_a = store.token_path(url);
        let path_b = store.token_path(url);
        assert_eq!(path_a, path_b, "same URL must produce same path");

        let path_other = store.token_path("https://other.example.com");
        assert_ne!(
            path_a, path_other,
            "different URLs must produce different paths"
        );
    }
}
