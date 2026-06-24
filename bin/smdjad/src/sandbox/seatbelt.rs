//! macOS Seatbelt isolation backend.
//!
//! Generates a `.sb` profile that grants read-write only under the confined
//! root (plus an ephemeral `/tmp`), keeps `.git` read-only, and applies the
//! network policy, then runs the command via `sandbox-exec -p <profile> sh -c`.
//!
//! `sandbox-exec` is deprecated-but-present on macOS and is the only
//! no-dependency confinement option there; `smj sandbox status` surfaces the
//! backend so operators can switch to Docker.

use std::path::Path;

use async_trait::async_trait;

use super::{resolve_confined_root, NetworkPolicy, SandboxBackend, EXEC_TIMEOUT_SECS};

/// The Seatbelt profile template (lives under `scripts/sandbox/`).
const PROFILE_TEMPLATE: &str = include_str!("../../../../scripts/sandbox/seatbelt.sb.template");

/// Executes commands under a generated macOS Seatbelt profile.
pub struct SeatbeltBackend {
    available: bool,
}

impl SeatbeltBackend {
    /// Probes for `sandbox-exec` on PATH.
    #[must_use]
    pub fn detect() -> Self {
        Self {
            available: which::which("sandbox-exec").is_ok(),
        }
    }

    /// Maps a [`NetworkPolicy`] to the Seatbelt network rule line.
    fn network_rule(policy: NetworkPolicy) -> &'static str {
        match policy {
            NetworkPolicy::None => "(deny network*)",
            // The kernel boundary permits outbound; the daemon's is_blocked_ip
            // floor keeps private/IMDS ranges unreachable.
            NetworkPolicy::Allowlist | NetworkPolicy::Open => "(allow network-outbound)",
        }
    }

    /// Generates the `.sb` profile string for `confined_root` / `git_dir` under
    /// `policy`.
    pub(crate) fn render_profile(
        confined_root: &Path,
        git_dir: &Path,
        policy: NetworkPolicy,
    ) -> String {
        PROFILE_TEMPLATE
            .replace("@CONFINED_ROOT@", &confined_root.display().to_string())
            .replace("@GIT_DIR@", &git_dir.display().to_string())
            .replace("@NETWORK_RULE@", Self::network_rule(policy))
    }
}

#[async_trait]
impl SandboxBackend for SeatbeltBackend {
    fn name(&self) -> &'static str {
        "seatbelt"
    }

    fn available(&self) -> bool {
        self.available
    }

    async fn exec(
        &self,
        cmd: &str,
        confined_root: &Path,
        policy: NetworkPolicy,
    ) -> Result<String, String> {
        if !self.available {
            return Err("seatbelt sandbox not available".into());
        }

        let (root, git) = resolve_confined_root(confined_root)?;
        // When .git is absent, point the read-only deny at the (non-existent)
        // path under the root; it is harmless.
        let git_dir = git.unwrap_or_else(|| root.join(".git"));
        let profile = Self::render_profile(&root, &git_dir, policy);
        let root_str = root
            .to_str()
            .ok_or("confined root contains non-UTF-8 bytes")?
            .to_owned();

        match tokio::time::timeout(
            std::time::Duration::from_secs(EXEC_TIMEOUT_SECS),
            tokio::process::Command::new("sandbox-exec")
                .args(["-p", &profile, "sh", "-c", cmd])
                .current_dir(&root_str)
                .output(),
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
#[cfg(target_os = "macos")]
mod tests {
    use super::*;

    #[test]
    fn seatbelt_profile_confines_writes_and_encodes_network_policy() {
        let root = Path::new("/tmp/smedja-confined");
        let git = Path::new("/tmp/smedja-confined/.git");

        // Confined writes: the profile grants write only under the root and
        // denies writes under .git.
        let p = SeatbeltBackend::render_profile(root, git, NetworkPolicy::None);
        assert!(p.contains("(deny default)"), "profile must deny by default");
        assert!(
            p.contains(
                r#"(allow file-write*
  (subpath "/tmp/smedja-confined"))"#
            ),
            "profile must grant write under the confined root; got:\n{p}"
        );
        assert!(
            p.contains(
                r#"(deny file-write*
  (subpath "/tmp/smedja-confined/.git"))"#
            ),
            "profile must keep .git read-only; got:\n{p}"
        );
        // No unsubstituted placeholders remain.
        assert!(
            !p.contains('@'),
            "all placeholders must be substituted; got:\n{p}"
        );

        // none → deny network*.
        assert!(
            p.contains("(deny network*)"),
            "none policy must deny network; got:\n{p}"
        );

        // allowlist / open → allow outbound.
        let pa = SeatbeltBackend::render_profile(root, git, NetworkPolicy::Allowlist);
        assert!(
            pa.contains("(allow network-outbound)"),
            "allowlist must allow outbound"
        );
        let po = SeatbeltBackend::render_profile(root, git, NetworkPolicy::Open);
        assert!(
            po.contains("(allow network-outbound)"),
            "open must allow outbound"
        );
    }
}
