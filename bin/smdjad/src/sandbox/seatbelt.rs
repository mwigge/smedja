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

use super::{
    resolve_confined_root, resolve_read_paths, NetworkPolicy, SandboxBackend, EXEC_TIMEOUT_SECS,
};

/// Secret subdirectories of `$HOME` that are explicitly read-denied.
const SECRET_SUBDIRS: &[&str] = &[".ssh", ".aws", ".config", ".gnupg"];

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

    /// Renders the `(allow file-read*)` block scoped to the system-dir
    /// allow-list (`resolve_read_paths()`), one `(subpath ...)` per directory.
    ///
    /// Replaces the former blanket `(allow file-read*)` so host secrets outside
    /// the allow-list stay unreadable.
    fn render_read_paths() -> String {
        let mut block = String::from("(allow file-read*");
        for path in resolve_read_paths() {
            block.push_str("\n  (subpath \"");
            block.push_str(&path.display().to_string());
            block.push_str("\")");
        }
        block.push(')');
        block
    }

    /// Renders the `(deny file-read*)` block over the user's secret subpaths
    /// (`$HOME/.ssh`, `.aws`, `.config`, `.gnupg`) for defence in depth.
    ///
    /// When `$HOME` is unset the block is a harmless no-op deny scoped to the
    /// secret names under `/` (which the default-deny already covers).
    fn render_read_deny() -> String {
        let home = std::env::var("HOME").unwrap_or_default();
        let mut block = String::from("(deny file-read*");
        for secret in SECRET_SUBDIRS {
            block.push_str("\n  (subpath \"");
            block.push_str(&home);
            block.push('/');
            block.push_str(secret);
            block.push_str("\")");
        }
        block.push(')');
        block
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
            .replace("@READ_PATHS@", &Self::render_read_paths())
            .replace("@READ_DENY@", &Self::render_read_deny())
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
                // Reap the sandboxed child on EXEC_TIMEOUT_SECS instead of
                // orphaning it when the timed-out `output()` future is dropped.
                .kill_on_drop(true)
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

    // ── 3.1 read confinement: deny secrets, allow system dirs ─────────────────

    #[test]
    fn profile_denies_secret_reads_and_allows_system_reads() {
        let root = Path::new("/tmp/smedja-confined");
        let git = Path::new("/tmp/smedja-confined/.git");
        let p = SeatbeltBackend::render_profile(root, git, NetworkPolicy::None);

        // The documented system-dir allow-list is present and operator-widenable.
        assert!(
            p.contains(r#"(subpath "/usr")"#),
            "system read paths must be allow-listed; got:\n{p}"
        );

        // The secret directories are explicitly read-denied. On Seatbelt the
        // last matching rule wins, so these denies are the read-confinement
        // boundary even though a broad read base loads the dyld shared cache.
        let home = std::env::var("HOME").unwrap_or_default();
        for secret in [".ssh", ".aws", ".config", ".gnupg"] {
            let needle = format!(r#"(subpath "{home}/{secret}")"#);
            assert!(
                p.contains(&needle),
                "secret path {secret} must be read-denied; got:\n{p}"
            );
        }
        assert!(
            p.contains("(deny file-read*"),
            "profile must contain a file-read deny over secrets; got:\n{p}"
        );

        // The deny block must come AFTER the broad allow so deny-precedence
        // (last match wins) actually carves the secrets back out.
        let allow_pos = p
            .find("(allow file-read*)")
            .expect("broad read base must exist for the dyld cache");
        let deny_pos = p
            .find("(deny file-read*")
            .expect("secret deny block must exist");
        assert!(
            deny_pos > allow_pos,
            "the secret deny must follow the broad allow so it wins; got:\n{p}"
        );

        // No unsubstituted placeholders remain.
        assert!(
            !p.contains('@'),
            "all placeholders must be substituted; got:\n{p}"
        );
    }

    // ── 3.1 (end-to-end) the rendered profile actually denies a secret read ───

    #[tokio::test]
    async fn seatbelt_exec_denies_secret_read_but_allows_shell() {
        let backend = SeatbeltBackend::detect();
        if !backend.available() {
            // sandbox-exec absent (unexpected on macOS): contract is downgrade.
            return;
        }

        // Hermetic HOME so the secret deny targets a path we control. Canonicalise
        // it: macOS temp dirs live under `/var/folders` which firmlinks to
        // `/private/var/folders`; the Seatbelt deny must use the same canonical
        // path the kernel resolves a read to, exactly as a real `$HOME` is
        // already canonical. The renderer reads HOME at render time inside `exec`.
        let fake_home_dir = tempfile::tempdir().unwrap();
        let fake_home = fake_home_dir.path().canonicalize().unwrap();
        let ssh = fake_home.join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::write(ssh.join("id_rsa"), "TOPSECRET-KEY").unwrap();

        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();

        // SAFETY: single-threaded test section; HOME restored before returning.
        let prev_home = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", &fake_home);
        }

        // A normal command runs (the shell + libs load under the broad base).
        let ok = backend
            .exec("printf alive", &root, NetworkPolicy::Open)
            .await;

        // Reading the secret is denied (deny-precedence wins).
        let secret_cmd = format!("cat {}", ssh.join("id_rsa").display());
        let denied = backend.exec(&secret_cmd, &root, NetworkPolicy::Open).await;

        unsafe {
            match prev_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }

        assert_eq!(
            ok.as_deref(),
            Ok("alive"),
            "the shell must still load and run; got: {ok:?}"
        );
        assert!(
            denied.is_err(),
            "reading a secret under $HOME/.ssh must be denied; got: {denied:?}"
        );
        if let Ok(text) = denied {
            assert!(
                !text.contains("TOPSECRET"),
                "secret content must not leak in output"
            );
        }
    }
}
