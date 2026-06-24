//! Linux Landlock isolation backend.
//!
//! Builds a Landlock ruleset that grants read-write only under the confined
//! root and read-execute across the rest of the filesystem, then applies it in
//! the child process (via `pre_exec`) before the command's `exec`.
//!
//! Landlock here enforces the *write* boundary — its strongest, most widely
//! supported guarantee (ABI v1, kernel ≥ 5.13): the command can read system
//! files and execute programs (so the shell and its shared libraries load), but
//! it can only create or modify files beneath the confined root. Writes to any
//! other path — including sibling directories under `/tmp` — are denied by the
//! kernel.
//!
//! Read confinement and network confinement are intentionally out of scope for
//! this backend: reads are broad (the additive ruleset cannot carve a read-only
//! hole under the read-execute `/` grant), and network egress is governed by the
//! daemon's `is_blocked_ip` floor, which keeps loopback/private/IMDS ranges
//! unreachable regardless of the requested `NetworkPolicy`.
//!
//! Availability is detected at startup; when Landlock is unavailable (kernel <
//! 5.13 or disabled) the backend reports `available() == false` so selection
//! downgrades to no-backend.

use std::path::Path;

use async_trait::async_trait;
use landlock::{
    Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
    ABI,
};

use super::{resolve_confined_root, NetworkPolicy, SandboxBackend, EXEC_TIMEOUT_SECS};

/// Executes commands confined by the Landlock LSM.
pub struct LandlockBackend {
    available: bool,
}

impl LandlockBackend {
    /// Detects Landlock support by building (not enforcing) a minimal ruleset.
    ///
    /// `create()` allocates the ruleset without restricting the current process
    /// (only `restrict_self` does that), so this probe is non-destructive. If
    /// the kernel cannot create the ruleset, the backend reports unavailable and
    /// selection downgrades. Runtime enforcement is still re-checked per command
    /// via the `RulesetStatus::NotEnforced` guard in `apply`.
    #[must_use]
    pub fn detect() -> Self {
        let available = Ruleset::default()
            .handle_access(AccessFs::from_all(ABI::V1))
            .and_then(Ruleset::create)
            .is_ok();
        Self { available }
    }

    /// Applies the filesystem ruleset confining writes to `root` in the current
    /// process. Intended to be called from `pre_exec` in the child.
    ///
    /// # Errors
    ///
    /// Returns `std::io::Error` when the ruleset cannot be created or applied.
    fn apply(root: &Path) -> std::io::Result<()> {
        let abi = ABI::V1;
        let map_err = |e: landlock::RulesetError| std::io::Error::other(format!("landlock: {e}"));
        // PathFd::new's error type is landlock-internal; map it to io::Error by
        // its Display so this io::Result function can propagate it uniformly.
        let open = |p: &Path| {
            PathFd::new(p).map_err(|e| std::io::Error::other(format!("landlock path: {e}")))
        };

        let created = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .map_err(map_err)?
            .create()
            .map_err(map_err)?
            // Read + execute across the whole filesystem so the shell and its
            // shared libraries can load and run.
            .add_rule(PathBeneath::new(
                open(Path::new("/"))?,
                AccessFs::from_read(abi),
            ))
            .map_err(map_err)?
            // Read-write only beneath the confined root. Because the ruleset is
            // additive, this widens the `/` read grant to read-write for this
            // subtree alone; every other path stays read-only.
            .add_rule(PathBeneath::new(open(root)?, AccessFs::from_all(abi)))
            .map_err(map_err)?;

        let status = created.restrict_self().map_err(map_err)?;
        if matches!(status.ruleset, RulesetStatus::NotEnforced) {
            return Err(std::io::Error::other(
                "landlock: ruleset not enforced by the kernel",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl SandboxBackend for LandlockBackend {
    fn name(&self) -> &'static str {
        "landlock"
    }

    fn available(&self) -> bool {
        self.available
    }

    async fn exec(
        &self,
        cmd: &str,
        confined_root: &Path,
        _policy: NetworkPolicy,
    ) -> Result<String, String> {
        if !self.available {
            return Err("landlock sandbox not available".into());
        }

        let (root, _git) = resolve_confined_root(confined_root)?;
        let root_for_child = root.clone();

        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", cmd]).current_dir(&root);

        // SAFETY: the closure runs in the forked child before `exec`; it only
        // calls async-signal-safe Landlock syscalls, and allocations performed
        // before fork are not freed here. Any error aborts the exec.
        unsafe {
            command.pre_exec(move || Self::apply(&root_for_child));
        }

        match tokio::time::timeout(
            std::time::Duration::from_secs(EXEC_TIMEOUT_SECS),
            command.output(),
        )
        .await
        {
            Err(_) => Err(format!(
                "sandbox: command timed out after {EXEC_TIMEOUT_SECS} seconds"
            )),
            Ok(Err(e)) => Err(e.to_string()),
            Ok(Ok(out)) if out.status.success() => {
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            Ok(Ok(out)) => Err(String::from_utf8_lossy(&out.stderr).into_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn landlock_ruleset_denies_write_outside_root() {
        let backend = LandlockBackend::detect();
        if !backend.available() {
            // Kernel without Landlock (e.g. older CI image): the contract is
            // that the backend reports unavailable and selection downgrades.
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        // A write inside the confined root succeeds.
        let inside = backend
            .exec("echo ok > inside.txt", &root, NetworkPolicy::None)
            .await;
        assert!(inside.is_ok(), "write inside root must succeed: {inside:?}");
        assert!(root.join("inside.txt").exists());

        // A write to a sibling outside the confined root is denied by the
        // kernel; the command fails (non-zero exit → Err).
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_target = outside_dir.path().join("escape.txt");
        let cmd = format!("echo escape > {}", outside_target.display());
        let outside = backend.exec(&cmd, &root, NetworkPolicy::None).await;
        assert!(
            outside.is_err(),
            "write outside confined root must be denied: {outside:?}"
        );
        assert!(
            !outside_target.exists(),
            "the escaping write must not have landed"
        );
    }
}
