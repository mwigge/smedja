//! Unix-domain socket location and lifecycle for the daemon listener.

use std::path::PathBuf;

/// Resolves the daemon's Unix socket path from `XDG_RUNTIME_DIR`, falling back
/// to `/tmp` (with a warning) when it is unset.
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
