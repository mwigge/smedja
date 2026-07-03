//! PKCE code generation (verifier/challenge) and the loopback redirect listener.

use super::token::PkceError;

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
pub(crate) fn code_challenge(verifier: &str) -> String {
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
pub(crate) fn generate_verifier() -> String {
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
pub(crate) async fn await_redirect(
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
}
