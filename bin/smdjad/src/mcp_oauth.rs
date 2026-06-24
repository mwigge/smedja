//! OAuth 2.0 PKCE flow for MCP HTTP server authentication.
//!
//! Implements the Authorization Code + PKCE (S256) flow as required by the MCP
//! specification for HTTP-based tool servers: a code verifier/challenge, a
//! loopback redirect listener with `state` validation, a token exchange, and a
//! refresh-token grant. Tokens are persisted by [`TokenStore`].
//!
//! At-rest token confidentiality relies on filesystem permissions (0600);
//! AES-256-GCM encryption is a separate hardening item.

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

impl std::error::Error for PkceError {}

/// Encodes `bytes` as unpadded base64url (RFC 4648 §5, no `=` padding).
///
/// Used for the PKCE `code_verifier` and `code_challenge`, both of which the
/// OAuth spec requires in unpadded base64url.
#[must_use]
fn base64url_no_pad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        let n =
            (u32::from(b0) << 16) | (u32::from(b1.unwrap_or(0)) << 8) | u32::from(b2.unwrap_or(0));
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if b1.is_some() {
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        if b2.is_some() {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        }
    }
    out
}

/// Derives the PKCE `code_challenge` from a `code_verifier` using S256:
/// `base64url(SHA256(verifier))` with no padding.
#[must_use]
fn code_challenge(verifier: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    base64url_no_pad(&digest)
}

/// Generates a high-entropy PKCE `code_verifier` (256 bits of randomness encoded
/// as unpadded base64url).
///
/// Randomness is sourced from two v4 UUIDs (each 122 bits, backed by the
/// platform CSPRNG) concatenated to 32 bytes, avoiding a new RNG dependency.
#[must_use]
fn generate_verifier() -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64url_no_pad(&bytes)
}

/// Parses the `code` and `state` query parameters from an HTTP request line of
/// the form `GET /callback?code=...&state=... HTTP/1.1`.
fn parse_callback_query(request_line: &str) -> (Option<String>, Option<String>) {
    let Some(target) = request_line.split_whitespace().nth(1) else {
        return (None, None);
    };
    let Some(query) = target.split_once('?').map(|(_, q)| q) else {
        return (None, None);
    };
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "code" => code = Some(value.to_owned()),
                "state" => state = Some(value.to_owned()),
                _ => {}
            }
        }
    }
    (code, state)
}

/// Awaits exactly one loopback redirect callback on `listener`, validates the
/// returned `state` against `expected_state`, and returns the authorization
/// `code`.
///
/// Binds the wall-clock `timeout`: if no callback arrives in time the flow is
/// [`PkceError::Cancelled`]. A mismatched or missing `state` is rejected.
///
/// # Errors
///
/// Returns [`PkceError::Cancelled`] on timeout and [`PkceError::Http`] on an
/// I/O failure, a malformed request, or a `state` mismatch.
async fn await_redirect(
    listener: tokio::net::TcpListener,
    expected_state: &str,
    timeout: std::time::Duration,
) -> Result<String, PkceError> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let accept = async {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|e| PkceError::Http(e.to_string()))?;

        let mut buf = [0u8; 2048];
        let n = socket
            .read(&mut buf)
            .await
            .map_err(|e| PkceError::Http(e.to_string()))?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let request_line = request.lines().next().unwrap_or_default();
        let (code, state) = parse_callback_query(request_line);

        // Always answer the browser so the tab can close, regardless of outcome.
        let _ = socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n\
                  smedja: authentication complete. You may close this tab.",
            )
            .await;
        let _ = socket.shutdown().await;

        match (code, state) {
            (Some(code), Some(state)) if state == expected_state => Ok(code),
            (_, Some(_)) => Err(PkceError::Http("state mismatch on redirect".to_owned())),
            _ => Err(PkceError::Http("malformed redirect callback".to_owned())),
        }
    };

    match tokio::time::timeout(timeout, accept).await {
        Ok(result) => result,
        Err(_) => Err(PkceError::Cancelled),
    }
}

/// The token endpoint for an MCP authorization server rooted at `server_url`.
fn token_endpoint(server_url: &str) -> String {
    format!("{}/token", server_url.trim_end_matches('/'))
}

/// The authorization endpoint for an MCP authorization server rooted at
/// `server_url`.
fn authorize_endpoint(server_url: &str) -> String {
    format!("{}/authorize", server_url.trim_end_matches('/'))
}

/// Exchanges an authorization `code` for a [`Token`] at `token_url` using the
/// PKCE `authorization_code` grant.
///
/// # Errors
///
/// Returns [`PkceError::Http`] on a transport failure, a non-success status, or
/// a response body that does not parse as a [`Token`].
async fn exchange_code(
    client: &reqwest::Client,
    token_url: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<Token, PkceError> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", code_verifier),
        ("redirect_uri", redirect_uri),
    ];
    let resp = client
        .post(token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| PkceError::Http(e.to_string()))?
        .error_for_status()
        .map_err(|e| PkceError::Http(e.to_string()))?;
    resp.json::<Token>()
        .await
        .map_err(|e| PkceError::Http(e.to_string()))
}

/// Performs a `refresh_token` grant against `server_url` and re-persists the
/// resulting [`Token`] via `store`.
///
/// # Errors
///
/// Returns [`PkceError::Http`] on a transport failure or when no refresh token
/// is present, and [`PkceError::Storage`] if the refreshed token cannot be
/// saved.
async fn refresh_token_with_store(
    server_url: &str,
    token: &Token,
    store: &TokenStore,
) -> Result<Token, PkceError> {
    let refresh = token
        .refresh_token
        .as_deref()
        .ok_or_else(|| PkceError::Http("no refresh token available".to_owned()))?;
    let params = [("grant_type", "refresh_token"), ("refresh_token", refresh)];
    let client = reqwest::Client::new();
    let resp = client
        .post(token_endpoint(server_url))
        .form(&params)
        .send()
        .await
        .map_err(|e| PkceError::Http(e.to_string()))?
        .error_for_status()
        .map_err(|e| PkceError::Http(e.to_string()))?;
    let refreshed: Token = resp
        .json()
        .await
        .map_err(|e| PkceError::Http(e.to_string()))?;
    store
        .save(server_url, &refreshed)
        .map_err(PkceError::Storage)?;
    Ok(refreshed)
}

/// Performs a `refresh_token` grant against `server_url`, re-persisting the new
/// token via the default [`TokenStore`].
///
/// # Errors
///
/// Returns [`PkceError`] on a transport failure or a storage failure.
#[must_use = "the refreshed token must be used or persisted"]
pub async fn refresh_token(server_url: &str, token: &Token) -> Result<Token, PkceError> {
    refresh_token_with_store(server_url, token, &TokenStore::default_store()).await
}

/// Starts an OAuth 2.0 Authorization Code + PKCE flow for `server_url`.
///
/// Generates a code verifier/challenge (S256), binds a loopback redirect
/// listener, logs the authorization URL for the operator to open, awaits the
/// redirect callback, exchanges the code for a [`Token`], and persists it via
/// the default [`TokenStore`].
///
/// # Errors
///
/// Returns [`PkceError::Cancelled`] if no callback arrives within the timeout,
/// [`PkceError::Http`] on a token-exchange failure, or [`PkceError::Storage`]
/// if the token cannot be saved.
#[must_use = "the issued token must be used or persisted"]
pub async fn start_pkce(server_url: &str) -> Result<Token, PkceError> {
    start_pkce_with_timeout(server_url, std::time::Duration::from_mins(5)).await
}

/// Runs the PKCE flow with an explicit redirect `timeout`.
///
/// # Errors
///
/// See [`start_pkce`].
async fn start_pkce_with_timeout(
    server_url: &str,
    timeout: std::time::Duration,
) -> Result<Token, PkceError> {
    let verifier = generate_verifier();
    let challenge = code_challenge(&verifier);
    let state = generate_verifier();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| PkceError::Http(e.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|e| PkceError::Http(e.to_string()))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let authorize_url = format!(
        "{}?response_type=code&code_challenge={}&code_challenge_method=S256\
         &redirect_uri={}&state={}",
        authorize_endpoint(server_url),
        challenge,
        redirect_uri,
        state,
    );
    tracing::info!(
        url = %authorize_url,
        "smedja.mcp.oauth: open this URL to authorise the MCP server"
    );

    let code = await_redirect(listener, &state, timeout).await?;

    let client = reqwest::Client::new();
    let token = exchange_code(
        &client,
        &token_endpoint(server_url),
        &code,
        &verifier,
        &redirect_uri,
    )
    .await?;

    TokenStore::default_store()
        .save(server_url, &token)
        .map_err(PkceError::Storage)?;
    Ok(token)
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
    fn code_challenge_matches_rfc7636_s256_vector() {
        // RFC 7636 Appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(code_challenge(verifier), expected);
    }

    #[tokio::test]
    async fn redirect_listener_accepts_matching_state_and_returns_code() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = "the-expected-state".to_owned();

        // Drive a single callback against the bound listener.
        let client = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            conn.write_all(
                b"GET /callback?code=auth-code-123&state=the-expected-state HTTP/1.1\r\n\
                  Host: localhost\r\n\r\n",
            )
            .await
            .unwrap();
        });

        let code = await_redirect(listener, &state, std::time::Duration::from_secs(2))
            .await
            .expect("matching state must yield the code");
        assert_eq!(code, "auth-code-123");
        client.await.unwrap();
    }

    #[tokio::test]
    async fn redirect_listener_rejects_mismatched_state() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = "expected".to_owned();

        let client = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            conn.write_all(
                b"GET /callback?code=c&state=forged HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .await
            .unwrap();
        });

        let result = await_redirect(listener, &state, std::time::Duration::from_secs(2)).await;
        assert!(
            result.is_err(),
            "mismatched state must be rejected; got: {result:?}"
        );
        client.await.unwrap();
    }

    #[tokio::test]
    async fn redirect_listener_times_out_to_cancelled() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        // No callback ever arrives → timeout maps to Cancelled.
        let result = await_redirect(listener, "state", std::time::Duration::from_millis(80)).await;
        assert!(matches!(result, Err(PkceError::Cancelled)));
    }

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
    async fn exchange_code_posts_grant_and_parses_token() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let token_url = format!("http://{addr}/token");

        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/token",
                axum::routing::post(|body: String| async move {
                    // The exchange must carry the authorization-code grant.
                    assert!(
                        body.contains("grant_type=authorization_code"),
                        "body: {body}"
                    );
                    assert!(body.contains("code=auth-xyz"), "body: {body}");
                    assert!(body.contains("code_verifier="), "body: {body}");
                    axum::Json(serde_json::json!({
                        "access_token": "issued-access",
                        "token_type": "Bearer",
                        "refresh_token": "issued-refresh",
                        "expires_in": 3600
                    }))
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let client = reqwest::Client::new();
        let token = exchange_code(
            &client,
            &token_url,
            "auth-xyz",
            "verifier-abc",
            "http://127.0.0.1:1/callback",
        )
        .await
        .expect("exchange must succeed");
        assert_eq!(token.access_token, "issued-access");
        assert_eq!(token.refresh_token.as_deref(), Some("issued-refresh"));
    }

    #[tokio::test]
    async fn exchange_code_transport_failure_maps_to_http_error() {
        let client = reqwest::Client::new();
        // Nothing is listening on this port → transport-level failure.
        let result = exchange_code(
            &client,
            "http://127.0.0.1:1/token",
            "code",
            "verifier",
            "http://127.0.0.1:1/callback",
        )
        .await;
        assert!(matches!(result, Err(PkceError::Http(_))));
    }

    #[tokio::test]
    async fn refresh_token_uses_refresh_grant_and_resaves() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_url = format!("http://{addr}");

        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/token",
                axum::routing::post(|body: String| async move {
                    assert!(body.contains("grant_type=refresh_token"), "body: {body}");
                    assert!(body.contains("refresh_token=old-refresh"), "body: {body}");
                    axum::Json(serde_json::json!({
                        "access_token": "fresh-access",
                        "token_type": "Bearer",
                        "refresh_token": "new-refresh",
                        "expires_in": 3600
                    }))
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let tmp = tempfile::tempdir().unwrap();
        let store = TokenStore::new(tmp.path().to_path_buf());
        let expired = Token {
            access_token: "stale-access".into(),
            token_type: "Bearer".into(),
            refresh_token: Some("old-refresh".into()),
            expires_in: Some(0),
        };

        let refreshed = refresh_token_with_store(&server_url, &expired, &store)
            .await
            .expect("refresh must succeed");
        assert_eq!(refreshed.access_token, "fresh-access");

        // The new token must be persisted under the server URL.
        let loaded = store.load(&server_url).unwrap().expect("token persisted");
        assert_eq!(loaded.access_token, "fresh-access");
        assert_eq!(loaded.refresh_token.as_deref(), Some("new-refresh"));
    }

    #[tokio::test]
    async fn start_pkce_drives_the_flow_and_times_out_to_cancelled() {
        // With no authorization server reachable and no callback driven, the
        // flow drives the redirect-and-exchange path and times out to
        // Cancelled — the old NotImplemented stub path is gone.
        let result =
            start_pkce_with_timeout("http://127.0.0.1:1", std::time::Duration::from_millis(80))
                .await;
        assert!(matches!(result, Err(PkceError::Cancelled)));
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
