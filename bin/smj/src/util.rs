//! Shared helpers: XDG path resolution and daemon socket connection.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use smedja_rpc::client::Client;

/// Returns the default path to the smedja ingot database.
///
/// The database lives under the XDG data directory so that it is isolated
/// from per-workspace `.smedja/` directories.
pub(crate) fn default_ingot_path() -> PathBuf {
    let data_home = std::env::var("XDG_DATA_HOME").map_or_else(
        |_| {
            std::env::var("HOME").map_or_else(
                |_| PathBuf::from(".local/share"),
                |h| PathBuf::from(h).join(".local/share"),
            )
        },
        PathBuf::from,
    );
    let dir = data_home.join("smedja");
    // Best-effort directory creation — open() will surface the error if it fails.
    let _ = std::fs::create_dir_all(&dir);
    dir.join("ingot.db")
}

pub(crate) fn default_socket_path() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(base).join("smdjad.sock")
}

/// Resolves the XDG config base directory.
///
/// Priority order:
/// 1. `$XDG_CONFIG_HOME` — if set and non-empty
/// 2. `$HOME/.config` — if `$HOME` is set
/// 3. `.config` — relative fallback (should not occur in practice)
pub(crate) fn xdg_config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME").map_or_else(
        |_| {
            std::env::var("HOME").map_or_else(
                |_| PathBuf::from(".config"),
                |h| PathBuf::from(h).join(".config"),
            )
        },
        PathBuf::from,
    )
}

/// Connects to the smdjad socket, mapping failures to the standard
/// "smdjad not running" context error.
pub(crate) async fn connect(sock: &Path) -> Result<Client> {
    Client::connect(sock)
        .await
        .with_context(|| format!("smdjad not running ({})", sock.display()))
}

/// Connects to the smdjad socket, printing a targeted diagnostic and exiting
/// with code 1 on failure.
///
/// Distinguishes:
/// - `NotFound` → daemon not started (socket file absent)
/// - `PermissionDenied` → socket exists but caller lacks access
/// - Other I/O errors → generic message
pub(crate) async fn connect_or_exit(sock: &Path) -> Client {
    match Client::connect(sock).await {
        Ok(c) => c,
        Err(e) => {
            let kind = e.downcast_ref::<std::io::Error>().map(std::io::Error::kind);
            match kind {
                Some(std::io::ErrorKind::NotFound) => {
                    eprintln!("error: smdjad is not running (socket not found)");
                    eprintln!("  Start it with: systemctl --user start smdjad");
                    eprintln!("  Or run directly: smdjad");
                }
                Some(std::io::ErrorKind::PermissionDenied) => {
                    eprintln!("error: permission denied connecting to smdjad socket");
                    eprintln!("  Check that you are running as the correct user");
                }
                _ => {
                    eprintln!("error: cannot connect to smdjad ({}): {e}", sock.display());
                }
            }
            std::process::exit(1);
        }
    }
}

/// Initialises tracing, honouring `SMEDJA_LOG_FORMAT`.
///
/// Accepts `text` (default) or `json`. Any unrecognised value falls back to
/// `text` and logs a warning once the subscriber is installed.
pub(crate) fn init_tracing() {
    let raw = std::env::var("SMEDJA_LOG_FORMAT").unwrap_or_default();
    let format = raw.trim().to_ascii_lowercase();
    let unrecognised = !matches!(format.as_str(), "" | "text" | "json");

    if format == "json" {
        tracing_subscriber::fmt().json().init();
    } else {
        tracing_subscriber::fmt::init();
    }

    if unrecognised {
        tracing::warn!(
            value = %raw,
            "unrecognised SMEDJA_LOG_FORMAT; falling back to 'text' (valid values: text, json)"
        );
    }
}
