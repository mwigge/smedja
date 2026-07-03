//! Filesystem paths, store openers, and daemon lifecycle helpers.
//!
//! Groups the socket/PID/secret path resolution, the ingot/vault openers, the
//! tracing initialiser, and the systemd readiness notification used by `main`.

use std::path::PathBuf;

use smedja_ingot::Ingot;
use smedja_rpc::{codes, RpcError};
use smedja_vault::Vault;

pub(crate) fn socket_path() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        tracing::warn!("XDG_RUNTIME_DIR not set; using /tmp for socket — set XDG_RUNTIME_DIR for a secure socket location");
        "/tmp".into()
    });
    PathBuf::from(base).join("smdjad.sock")
}

/// RAII guard that removes the Unix socket file when dropped.
///
/// This ensures the socket is cleaned up on both clean shutdown and error
/// propagation (e.g. when `server.serve()` returns `Err` and `?` exits early).
pub(crate) struct SocketGuard {
    pub(crate) path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(crate) fn open_ingot() -> anyhow::Result<Ingot> {
    // Try to open the persistent store under ~/.local/share/smedja/smedja.db.
    // If the data directory cannot be created, fall back to in-memory.
    let data_dir = dirs_home()
        .map(|h| h.join(".local").join("share").join("smedja"))
        .filter(|d| std::fs::create_dir_all(d).is_ok());

    if let Some(dir) = data_dir {
        let db_path = dir.join("smedja.db");
        let ingot = Ingot::open(&db_path).map_err(anyhow::Error::from)?;
        // Ensure the database file is only readable by the owner.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(ingot)
    } else {
        tracing::error!("cannot create data directory; using in-memory store — all session data will be lost on restart");
        Ingot::open_in_memory().map_err(anyhow::Error::from)
    }
}

pub(crate) fn open_vault() -> Vault {
    // Mirror the ingot path: ~/.local/share/smedja/vault.db.
    // Falls back to an in-memory vault if the directory cannot be created.
    let vault_path = dirs_home()
        .map(|h| h.join(".local").join("share").join("smedja"))
        .filter(|d| std::fs::create_dir_all(d).is_ok())
        .map(|dir| dir.join("vault.db"));

    if let Some(path) = vault_path {
        match Vault::open(&path) {
            Ok(v) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
                return v;
            }
            Err(e) => tracing::warn!(error = %e, "vault open failed; using in-memory vault"),
        }
    } else {
        tracing::warn!("cannot create vault data directory; using in-memory vault");
    }
    Vault::open_in_memory().expect("in-memory vault must open")
}

/// Returns the user's home directory, or `None` if it cannot be determined.
pub(crate) fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

pub(crate) fn ingot_err(e: &smedja_ingot::IngotError) -> RpcError {
    RpcError::new(codes::INTERNAL_ERROR, e.to_string())
}

pub(crate) fn missing_param(name: &str) -> RpcError {
    RpcError::new(
        codes::INVALID_PARAMS,
        format!("missing required param: {name}"),
    )
}

/// Writes the ACP auth token to the runtime secret file with 0o600 permissions.
///
/// Path preference: `$XDG_RUNTIME_DIR/smdjad.secret` → `$HOME/.cache/smdjad.secret`.
/// Resolves the private path for the ACP secret from the runtime/home inputs, or
/// `None` when only a world-traversable location (e.g. `/tmp`) would be available.
///
/// The secret is never written to `/tmp`: a world-traversable directory lets any
/// local user learn the secret file's existence and (with lax permissions)
/// content, so the daemon refuses rather than falling back there.
fn acp_secret_path_from(
    xdg_runtime_dir: Option<&str>,
    home: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    if let Some(dir) = xdg_runtime_dir {
        if !dir.is_empty() {
            return Some(std::path::PathBuf::from(dir).join("smdjad.secret"));
        }
    }
    home.map(|h| h.join(".cache").join("smdjad.secret"))
}

pub(crate) fn write_acp_secret(token: &str) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    let home = dirs_home();
    let Some(secret_path) = acp_secret_path_from(xdg.as_deref(), home.as_deref()) else {
        tracing::error!(
            "no private directory for the ACP secret (set XDG_RUNTIME_DIR or HOME); \
             refusing to write it to a world-traversable location like /tmp"
        );
        return;
    };

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&secret_path)
    {
        let _ = f.write_all(token.as_bytes());
    } else {
        tracing::warn!(path = %secret_path.display(), "could not write ACP secret file");
    }
}

/// Resolves the daemon workspace root from an explicit env value and the current
/// directory, never returning the bare relative `"."`.
fn resolve_workspace_root_from(env: Option<String>, cwd: std::path::PathBuf) -> std::path::PathBuf {
    match env {
        Some(p) if !p.is_empty() => std::path::PathBuf::from(p),
        _ => cwd,
    }
}

/// Resolves the workspace root: `SMEDJA_WORKSPACE` if set, else the absolute
/// current directory. The relative `"."` default is avoided because its meaning
/// depends on the launcher's working directory under a supervisor.
pub(crate) fn resolve_workspace_root() -> std::path::PathBuf {
    let env = std::env::var("SMEDJA_WORKSPACE").ok();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    resolve_workspace_root_from(env, cwd)
}

/// Signals `READY=1` to systemd via `$NOTIFY_SOCKET` (for `Type=notify` units),
/// after the socket is bound and the database is open. A no-op when not run
/// under systemd (the variable is absent) or off Linux.
#[cfg(target_os = "linux")]
pub(crate) fn sd_notify_ready() {
    let Ok(path) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else {
        return;
    };
    let sent = if let Some(name) = path.strip_prefix('@') {
        // Abstract namespace socket (common for user services).
        use std::os::linux::net::SocketAddrExt as _;
        std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .and_then(|addr| sock.send_to_addr(b"READY=1", &addr))
            .is_ok()
    } else {
        sock.send_to(b"READY=1", &path).is_ok()
    };
    if sent {
        tracing::debug!("notified systemd: READY=1");
    }
}

/// No-op readiness notification off Linux (systemd is Linux-only).
#[cfg(not(target_os = "linux"))]
pub(crate) fn sd_notify_ready() {}

/// Initialises the tracing subscriber, honouring `SMEDJA_LOG_FORMAT`.
///
/// `text` (default) uses the human-readable formatter; `json` emits structured
/// JSON for log-ingestion pipelines (Loki, `OpenSearch`); an unrecognised value
/// falls back to text with a warning.
pub(crate) fn init_tracing() {
    match std::env::var("SMEDJA_LOG_FORMAT").as_deref() {
        Ok("json") => tracing_subscriber::fmt().json().init(),
        Ok("text" | "") | Err(_) => tracing_subscriber::fmt().init(),
        Ok(other) => {
            tracing_subscriber::fmt().init();
            tracing::warn!(format = other, "unrecognised SMEDJA_LOG_FORMAT; using text");
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn acp_secret_path_prefers_private_dirs_and_refuses_tmp() {
        use std::path::Path;
        assert_eq!(
            super::acp_secret_path_from(Some("/run/user/501"), None),
            Some(std::path::PathBuf::from("/run/user/501/smdjad.secret"))
        );
        assert_eq!(
            super::acp_secret_path_from(None, Some(Path::new("/home/u"))),
            Some(std::path::PathBuf::from("/home/u/.cache/smdjad.secret"))
        );
        // No XDG_RUNTIME_DIR and no HOME → refuse (would only be /tmp).
        assert_eq!(super::acp_secret_path_from(None, None), None);
        assert_eq!(super::acp_secret_path_from(Some(""), None), None);
    }

    #[test]
    fn resolve_workspace_root_uses_explicit_env_else_absolute_cwd() {
        let cwd = std::path::PathBuf::from("/abs/cwd");
        assert_eq!(
            super::resolve_workspace_root_from(Some("/ws".to_owned()), cwd.clone()),
            std::path::PathBuf::from("/ws")
        );
        // Unset/empty → the (absolute) cwd, never the relative ".".
        let got = super::resolve_workspace_root_from(None, cwd.clone());
        assert_eq!(got, cwd);
        assert_ne!(got, std::path::PathBuf::from("."));
    }
}
