//! Runtime filesystem locations: the ACP secret file and the workspace root.

use crate::store::dirs_home;

/// Writes the ACP auth token to the runtime secret file with 0o600 permissions.
///
/// Path preference: `$XDG_RUNTIME_DIR/smdjad.secret` → `$HOME/.cache/smdjad.secret`
/// → `/tmp/smdjad.secret`.
/// Resolves the private path for the ACP secret from the runtime/home inputs, or
/// `None` when only a world-traversable location (e.g. `/tmp`) would be available.
///
/// The secret is never written to `/tmp`: a world-traversable directory lets any
/// local user learn the secret file's existence and (with lax permissions)
/// content, so the daemon refuses rather than falling back there.
pub(crate) fn acp_secret_path_from(
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
pub(crate) fn resolve_workspace_root_from(
    env: Option<String>,
    cwd: std::path::PathBuf,
) -> std::path::PathBuf {
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
