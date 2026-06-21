//! SSH multiplexer — connects to a remote host via russh, checks for a remote
//! `smedja-term-mux` daemon, and forwards the remote smdjad Unix socket to a
//! local path via a `direct-streamlocal` channel bridge.

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context as _};
use russh::client;
use russh::keys::{self, PrivateKeyWithHashAlg};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::info;

// ── Handler ───────────────────────────────────────────────────────────────────

struct AcceptAllHandler;

impl client::Handler for AcceptAllHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // ponytail: accept any host key — proper known-hosts checking is future work
        Ok(true)
    }
}

// ── Public types ──────────────────────────────────────────────────────────────

/// Active SSH session handle, sharable across async tasks.
pub struct SshMuxClient {
    handle: Arc<Mutex<client::Handle<AcceptAllHandler>>>,
}

// ── parse_host_user ───────────────────────────────────────────────────────────

/// Splits a `[user@]host` string into `(username, hostname)`.
///
/// Falls back to the `USER` environment variable when no `@` is present.
#[must_use]
pub fn parse_host_user(input: &str) -> (String, String) {
    if let Some((user, host)) = input.split_once('@') {
        (user.to_owned(), host.to_owned())
    } else {
        let user = std::env::var("USER").unwrap_or_else(|_| "root".to_owned());
        (user, input.to_owned())
    }
}

// ── connect ───────────────────────────────────────────────────────────────────

/// Establishes an SSH connection to `host:port` authenticated as `username`.
///
/// Tries every key found in `~/.ssh/` (ed25519, rsa, ecdsa in that order).
///
/// # Errors
///
/// Returns an error when the TCP connection, key loading, or authentication
/// fails.
pub async fn connect(host: &str, port: u16, username: &str) -> anyhow::Result<SshMuxClient> {
    let config = Arc::new(client::Config::default());
    let addr = format!("{host}:{port}");
    let mut handle = client::connect(config, addr.as_str(), AcceptAllHandler)
        .await
        .context("TCP connect")?;

    // Try common key file names in order.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned());
    let key_candidates = [
        format!("{home}/.ssh/id_ed25519"),
        format!("{home}/.ssh/id_rsa"),
        format!("{home}/.ssh/id_ecdsa"),
    ];

    let mut authenticated = false;
    for path in &key_candidates {
        let Ok(key) = keys::load_secret_key(path, None) else {
            continue;
        };
        let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key), None);
        let result = handle
            .authenticate_publickey(username, key_with_alg)
            .await
            .context("publickey auth")?;
        if result.success() {
            info!(key = %path, "SSH authenticated");
            authenticated = true;
            break;
        }
    }

    if !authenticated {
        bail!("SSH authentication failed for {username}@{host} — no accepted key found");
    }

    Ok(SshMuxClient {
        handle: Arc::new(Mutex::new(handle)),
    })
}

// ── SshMuxClient ──────────────────────────────────────────────────────────────

impl SshMuxClient {
    /// Runs `which smedja-term-mux` on the remote host.
    ///
    /// # Errors
    ///
    /// Returns an error when the SSH exec channel cannot be opened or the
    /// daemon binary is not found on the remote `PATH`.
    pub async fn ensure_mux_daemon(&self) -> anyhow::Result<()> {
        let output = self.exec("which smedja-term-mux").await?;
        if output.trim().is_empty() {
            bail!("smedja-term-mux not found on remote PATH");
        }
        info!(path = %output.trim(), "smedja-term-mux found on remote");
        Ok(())
    }

    /// Opens a local Unix listener at `local_socket` and bridges every incoming
    /// connection to the remote smdjad socket via a `direct-streamlocal`
    /// channel.
    ///
    /// The function returns immediately; the bridge runs in detached Tokio tasks.
    ///
    /// # Errors
    ///
    /// Returns an error when the local Unix socket cannot be bound.
    pub fn open_local_tunnel(&self, local_socket: &Path) -> anyhow::Result<()> {
        // Remove stale socket file so bind succeeds.
        let _ = std::fs::remove_file(local_socket);
        let listener = UnixListener::bind(local_socket)
            .with_context(|| format!("bind Unix socket {}", local_socket.display()))?;

        let handle = Arc::clone(&self.handle);
        // Remote socket path — smdjad's default.
        let remote_socket = "/run/smdjad/smdjad.sock".to_owned();

        tokio::spawn(async move {
            loop {
                let Ok((local_stream, _)) = listener.accept().await else {
                    break;
                };
                let handle = Arc::clone(&handle);
                let remote_socket = remote_socket.clone();
                tokio::spawn(async move {
                    let channel = {
                        let guard = handle.lock().await;
                        match guard.channel_open_direct_streamlocal(remote_socket).await {
                            Ok(ch) => ch,
                            Err(e) => {
                                tracing::error!("direct-streamlocal open failed: {e}");
                                return;
                            }
                        }
                    };

                    let (mut read_half, write_half) = channel.split();
                    let mut ssh_reader = read_half.make_reader();
                    let mut ssh_writer = write_half.make_writer();
                    let (mut local_r, mut local_w) = tokio::io::split(local_stream);

                    let a = tokio::io::copy(&mut ssh_reader, &mut local_w);
                    let b = tokio::io::copy(&mut local_r, &mut ssh_writer);
                    // Run both directions concurrently; stop when either ends.
                    tokio::select! {
                        _ = a => {}
                        _ = b => {}
                    }
                });
            }
        });

        info!(
            socket = %local_socket.display(),
            "local tunnel listening"
        );
        Ok(())
    }

    /// Runs `cmd` on the remote host and returns stdout as a `String`.
    async fn exec(&self, cmd: &str) -> anyhow::Result<String> {
        let channel = {
            let guard = self.handle.lock().await;
            guard
                .channel_open_session()
                .await
                .context("open exec session")?
        };
        channel.exec(true, cmd).await.context("exec request")?;

        let (mut read_half, _write_half) = channel.split();
        let mut reader = read_half.make_reader();
        let mut buf = Vec::new();
        tokio::io::copy(&mut reader, &mut buf)
            .await
            .context("reading exec output")?;
        String::from_utf8(buf).context("exec output is not UTF-8")
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::parse_host_user;

    #[test]
    fn parse_host_user_with_at_sign() {
        let (user, host) = parse_host_user("alice@example.com");
        assert_eq!(user, "alice");
        assert_eq!(host, "example.com");
    }

    #[test]
    fn parse_host_user_without_at_sign_uses_env_user() {
        // Force USER env var to a known value for determinism.
        std::env::set_var("USER", "testuser");
        let (user, host) = parse_host_user("myhost");
        assert_eq!(user, "testuser");
        assert_eq!(host, "myhost");
    }
}
