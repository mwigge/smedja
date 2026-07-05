//! Shared filesystem contract for the backends: the read allow-list and the
//! confined-root resolution used identically across Docker/Seatbelt/Landlock.

use std::path::{Path, PathBuf};

/// System directories a sandboxed command may *read* from by default.
///
/// This is the read allow-list floor: the directories a shell and common tools
/// need to load (binaries and shared libraries) and resolve basic system
/// configuration. It deliberately excludes the user's home directory and its
/// secret subpaths (`~/.ssh`, `~/.aws`, `~/.config`, `~/.gnupg`) so a sandboxed
/// command cannot read host credentials. The macOS-only entries
/// (`/System`, `/Library`, `/private/var/db/dyld`) cover the dyld shared cache
/// the Seatbelt backend needs. Operators widen the list via
/// `SMEDJA_SANDBOX_READ_PATHS`; they never shrink it.
pub(crate) const DEFAULT_READ_PATHS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc",
    "/opt",
    #[cfg(target_os = "macos")]
    "/System",
    #[cfg(target_os = "macos")]
    "/Library",
    #[cfg(target_os = "macos")]
    "/private/var/db/dyld",
];

/// Resolves the read allow-list for sandboxed commands.
///
/// Starts from [`DEFAULT_READ_PATHS`] and *appends* the colon-separated paths in
/// `SMEDJA_SANDBOX_READ_PATHS` (operators widen, never replace). Paths that do
/// not exist on the host are skipped so a missing default (for example
/// `/lib64` on macOS) is not an error. Backends share this one source of truth
/// so the read floor is identical across Landlock and Seatbelt.
#[must_use]
pub(crate) fn resolve_read_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = DEFAULT_READ_PATHS.iter().map(PathBuf::from).collect();

    if let Ok(extra) = std::env::var("SMEDJA_SANDBOX_READ_PATHS") {
        for entry in extra.split(':') {
            let entry = entry.trim();
            if !entry.is_empty() {
                paths.push(PathBuf::from(entry));
            }
        }
    }

    // Skip paths that do not exist on this host (no error); backends that open
    // an fd per path would otherwise fail on an absent default.
    paths.retain(|p| p.exists());
    paths
}

/// Canonicalises `confined_root` and resolves the writable subtree, the
/// read-only `.git` path (when present), and the path string used for mounts.
///
/// Shared by the backends so they agree on the confined-root contract.
///
/// # Errors
///
/// Returns `Err` when the root cannot be canonicalised, is outside an
/// `SMEDJA_WORKSPACE_ROOT` (when set), or contains non-UTF-8 bytes.
pub(crate) fn resolve_confined_root(
    confined_root: &Path,
) -> Result<(std::path::PathBuf, Option<std::path::PathBuf>), String> {
    let root = confined_root
        .canonicalize()
        .map_err(|e| format!("invalid confined root: {e}"))?;

    if let Ok(allowed) = std::env::var("SMEDJA_WORKSPACE_ROOT") {
        let allowed = std::path::PathBuf::from(allowed);
        if !root.starts_with(&allowed) {
            return Err(format!(
                "confined root {} is outside allowed root {}",
                root.display(),
                allowed.display()
            ));
        }
    }

    // Read-confinement guard: `.git` is bind-mounted (Docker) / read-allowed
    // (Seatbelt) into the sandbox, so a symlinked `.git` would let it resolve to
    // an arbitrary host path (e.g. `ln -s ~/.ssh .git`) and expose host secrets
    // at `/workspace/.git`. Use `symlink_metadata` (which does NOT follow the
    // final component) and refuse to treat a symlinked `.git` as the git dir;
    // additionally require that the resolved path stays under the confined root.
    let git_dir = root.join(".git");
    let git = match std::fs::symlink_metadata(&git_dir) {
        // A symlinked `.git` is never mounted — it is the escape vector.
        Ok(meta) if meta.file_type().is_symlink() => None,
        // A real `.git` (directory or worktree pointer file) is mounted only
        // when it canonicalises to a path still inside the confined root.
        Ok(_) => match git_dir.canonicalize() {
            Ok(canon) if canon.starts_with(&root) => Some(canon),
            _ => None,
        },
        // Absent `.git` (or unreadable): nothing to mount.
        Err(_) => None,
    };
    Ok((root, git))
}
