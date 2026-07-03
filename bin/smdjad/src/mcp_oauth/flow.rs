//! Authorization-code and refresh-token grants plus the top-level PKCE entry points.

use super::pkce::{await_redirect, code_challenge, generate_verifier};
use super::store::TokenStore;
use super::token::{PkceError, Token};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
