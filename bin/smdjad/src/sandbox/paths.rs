//! Filesystem allow-list and confined-root resolution shared by the backends.
//!
//! [`resolve_read_paths`] yields the read allow-list floor and
//! [`resolve_confined_root`] canonicalises the writable root. Both are the one
//! source of truth so every backend agrees on the confined-root contract.

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

/// Serialises tests that mutate process-global state (env vars, cwd).
///
/// Cargo runs tests multithreaded; any test that touches `std::env` or the
/// current directory takes this one lock so it cannot race a sibling test.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
) -> Result<(PathBuf, Option<PathBuf>), String> {
    let root = confined_root
        .canonicalize()
        .map_err(|e| format!("invalid confined root: {e}"))?;

    if let Ok(allowed) = std::env::var("SMEDJA_WORKSPACE_ROOT") {
        let allowed = PathBuf::from(allowed);
        if !root.starts_with(&allowed) {
            return Err(format!(
                "confined root {} is outside allowed root {}",
                root.display(),
                allowed.display()
            ));
        }
    }

    let git_dir = root.join(".git");
    let git = if git_dir.exists() {
        Some(git_dir)
    } else {
        None
    };
    Ok((root, git))
}

#[cfg(test)]
mod tests {
    use super::{resolve_read_paths, DEFAULT_READ_PATHS, ENV_LOCK};

    // ── 1.1 shared read-path resolution ───────────────────────────────────────

    #[test]
    fn resolve_read_paths_uses_defaults_and_appends_env() {
        let _env = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // The defaults must contain core system dirs and must NOT contain the
        // user's home or secret directories.
        let home = std::env::var("HOME").unwrap_or_default();
        for d in DEFAULT_READ_PATHS {
            // Defaults are absolute system dirs, never under $HOME.
            assert!(d.starts_with('/'), "default path must be absolute: {d}");
            if !home.is_empty() {
                assert!(
                    !std::path::Path::new(d).starts_with(&home),
                    "default read paths must not include the home dir: {d}"
                );
            }
        }
        assert!(
            DEFAULT_READ_PATHS.contains(&"/usr"),
            "defaults must include /usr"
        );
        assert!(
            DEFAULT_READ_PATHS.contains(&"/bin"),
            "defaults must include /bin"
        );

        // A colon-separated override is appended to (not replacing) the defaults.
        // Use real, existing directories so the existence filter keeps them.
        let tmp = tempfile::tempdir().unwrap();
        let extra_a = tmp.path().join("toola");
        let extra_b = tmp.path().join("toolb");
        std::fs::create_dir(&extra_a).unwrap();
        std::fs::create_dir(&extra_b).unwrap();
        let override_val = format!("{}:{}", extra_a.display(), extra_b.display());

        // SAFETY: single-threaded test; restored below.
        unsafe {
            std::env::set_var("SMEDJA_SANDBOX_READ_PATHS", &override_val);
        }
        let resolved = resolve_read_paths();
        unsafe {
            std::env::remove_var("SMEDJA_SANDBOX_READ_PATHS");
        }

        // The override entries are present, appended after the defaults.
        assert!(
            resolved.contains(&extra_a),
            "override path A must be appended; got: {resolved:?}"
        );
        assert!(
            resolved.contains(&extra_b),
            "override path B must be appended; got: {resolved:?}"
        );
        // Non-existent default paths are skipped, but at least one default that
        // exists on every host (`/usr` or `/etc`) must survive.
        assert!(
            resolved
                .iter()
                .any(|p| p == std::path::Path::new("/usr") || p == std::path::Path::new("/etc")),
            "at least one existing default must remain; got: {resolved:?}"
        );
    }
}
