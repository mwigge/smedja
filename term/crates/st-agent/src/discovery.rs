//! Socket discovery and per-pane environment-variable injection.

use std::path::{Path, PathBuf};

use uuid::Uuid;

// ─────────────────────────────────────────────────────────────────────────────
// Socket discovery
// ─────────────────────────────────────────────────────────────────────────────

/// Returns the path to the smdjad Unix domain socket.
///
/// Uses `$XDG_RUNTIME_DIR/smdjad.sock` when `XDG_RUNTIME_DIR` is set,
/// otherwise falls back to `/tmp/smdjad.sock`.
#[must_use]
pub fn smdjad_socket_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(xdg).join("smdjad.sock")
    } else {
        PathBuf::from("/tmp/smdjad.sock")
    }
}

/// Returns `true` if the smdjad socket exists on the filesystem.
pub async fn socket_exists() -> bool {
    tokio::fs::metadata(smdjad_socket_path()).await.is_ok()
}

/// Returns the agent-event push socket path: `<rpc_path>.agent`.
#[must_use]
pub fn agent_socket_path(rpc_path: &Path) -> PathBuf {
    let name = rpc_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut p = rpc_path.to_path_buf();
    p.set_file_name(format!("{name}.agent"));
    p
}

// ─────────────────────────────────────────────────────────────────────────────
// Pane environment-variable injection
// ─────────────────────────────────────────────────────────────────────────────

/// Returns the `(key, value)` pair to inject into a child process environment
/// so that the agent inside the pane can report its pane identity to smdjad.
#[must_use]
pub fn pane_env_var(pane_id: &Uuid) -> (String, String) {
    ("SMEDJA_TERM_PANE".into(), pane_id.to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smdjad_socket_path_uses_xdg_runtime_dir() {
        // Temporarily set XDG_RUNTIME_DIR; restore afterward.
        let _guard = EnvGuard::set("XDG_RUNTIME_DIR", "/run/user/1000");
        let path = smdjad_socket_path();
        assert_eq!(path, PathBuf::from("/run/user/1000/smdjad.sock"));
    }

    #[test]
    fn socket_path_matches_smdjad() {
        // st-agent and smdjad must agree on the socket path for a given XDG_RUNTIME_DIR.
        // This test verifies the st-agent path matches the expected format.
        let _guard = EnvGuard::set("XDG_RUNTIME_DIR", "/run/user/9999");
        let path = smdjad_socket_path();
        assert_eq!(
            path.to_str().unwrap(),
            "/run/user/9999/smdjad.sock",
            "socket path must be $XDG_RUNTIME_DIR/smdjad.sock"
        );
        // Confirm no subdirectory: path should not contain /smedja/
        assert!(
            !path.to_str().unwrap().contains("/smedja/"),
            "socket path must not contain /smedja/ subdirectory"
        );
    }

    #[test]
    fn smdjad_socket_path_falls_back_to_tmp() {
        let _guard = EnvGuard::remove("XDG_RUNTIME_DIR");
        let path = smdjad_socket_path();
        assert_eq!(path, PathBuf::from("/tmp/smdjad.sock"));
    }

    #[test]
    fn pane_env_var_returns_correct_key() {
        let id = Uuid::new_v4();
        let (key, val) = pane_env_var(&id);
        assert_eq!(key, "SMEDJA_TERM_PANE");
        assert_eq!(val, id.to_string());
    }

    #[test]
    fn agent_socket_path_appends_dot_agent() {
        let p = agent_socket_path(std::path::Path::new("/run/smdjad.sock"));
        assert_eq!(
            p,
            std::path::PathBuf::from("/run/smdjad.sock.agent"),
            "expected .agent suffix"
        );
    }

    // ── Test helpers ─────────────────────────────────────────────────────

    /// Serialises env-mutating tests: cargo runs tests in the same process on
    /// multiple threads, so concurrent set/remove of a shared var would race.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that sets or removes an environment variable and restores the
    /// original value on drop.  Holds [`ENV_LOCK`] for its lifetime so env
    /// mutation is serialised across concurrently-running tests.
    struct EnvGuard {
        key: String,
        previous: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set(key: &str, value: &str) -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous = std::env::var(key).ok();
            // SAFETY: env mutation serialised by ENV_LOCK; restored on drop.
            unsafe { std::env::set_var(key, value) };
            Self {
                key: key.to_owned(),
                previous,
                _lock: lock,
            }
        }

        fn remove(key: &str) -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous = std::env::var(key).ok();
            // SAFETY: env mutation serialised by ENV_LOCK; restored on drop.
            unsafe { std::env::remove_var(key) };
            Self {
                key: key.to_owned(),
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => unsafe { std::env::set_var(&self.key, v) },
                None => unsafe { std::env::remove_var(&self.key) },
            }
        }
    }
}
