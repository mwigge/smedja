//! Docker isolation backend.
//!
//! Runs the command inside an ephemeral container with the confined root
//! bind-mounted read-write, `.git` shadowed read-only, a dropped capability
//! set, a read-only root filesystem, and a `/tmp` tmpfs. The network is
//! configured per [`NetworkPolicy`].
//!
//! Read confinement is *structural* here: only the confined root is bind-mounted
//! into the container, so the host home directory and its secret subpaths
//! (`~/.ssh`, `~/.aws`, `~/.config`, `~/.gnupg`) are never present in the
//! container's filesystem and cannot be read. This is the Docker read floor; no
//! per-path read rules are needed because the host filesystem is simply absent.

use std::path::Path;

use async_trait::async_trait;

use super::{resolve_confined_root, NetworkPolicy, SandboxBackend, EXEC_TIMEOUT_SECS};

/// Executes commands inside an ephemeral Docker container.
pub struct DockerBackend {
    /// `true` when Docker is reachable and the sandbox image inspects clean.
    available: bool,
    /// The sandbox image reference (tag or digest).
    image: String,
}

impl DockerBackend {
    /// Probes Docker availability and the sandbox image.
    ///
    /// Uses a blocking `std::process::Command` for the image inspect; this runs
    /// once at startup before the runtime is accepting work.
    #[must_use]
    pub fn detect() -> Self {
        if which::which("docker").is_err() {
            return Self {
                available: false,
                image: String::new(),
            };
        }

        let image = std::env::var("SMEDJA_SANDBOX_IMAGE")
            .unwrap_or_else(|_| "smedja-sandbox:latest".to_owned());
        if image.ends_with(":latest") {
            tracing::warn!(
                %image,
                "sandbox image uses :latest tag — pin to a digest with SMEDJA_SANDBOX_IMAGE=smedja-sandbox@sha256:<digest> for supply-chain safety"
            );
        }

        let out = std::process::Command::new("docker")
            .args(["image", "inspect", "--format", "{{.Id}}", &image])
            .output();

        match out {
            Ok(o) if o.status.success() => {
                let digest = String::from_utf8_lossy(&o.stdout).trim().to_owned();
                tracing::info!(%image, digest = %digest, "sandbox image verified");
                Self {
                    available: true,
                    image,
                }
            }
            _ => {
                tracing::warn!(%image, "sandbox image not found; run `smj sandbox build`");
                Self {
                    available: false,
                    image,
                }
            }
        }
    }

    /// Maps a [`NetworkPolicy`] to the Docker `--network` value.
    ///
    /// `none` isolates the container network entirely. `allowlist`/`open` use
    /// the default bridge; the `is_blocked_ip` floor is enforced by the daemon
    /// before egress is attempted (private/IMDS ranges stay unreachable).
    fn network_arg(policy: NetworkPolicy) -> &'static str {
        match policy {
            NetworkPolicy::None => "none",
            NetworkPolicy::Allowlist | NetworkPolicy::Open => "bridge",
        }
    }

    /// Builds the full `docker run …` argument vector for a confined execution.
    ///
    /// Only `confined_root` is bind-mounted read-write at `/workspace` (the
    /// structural read floor — no host home is mounted); `git_dir`, when
    /// present, is shadowed read-only at `/workspace/.git`. Pure string logic so
    /// the mount set and `--network` value are unit-testable without invoking
    /// Docker.
    #[must_use]
    pub(crate) fn build_run_args(
        image: &str,
        confined_root: &str,
        git_dir: Option<&str>,
        cmd: &str,
        policy: NetworkPolicy,
    ) -> Vec<String> {
        let network = Self::network_arg(policy);
        let mut args: Vec<String> = [
            "run",
            "--rm",
            "--network",
            network,
            "--cpus",
            "0.5",
            "--memory",
            "256m",
            "--pids-limit",
            "64",
            "--stop-timeout",
            "30",
            "--security-opt",
            "no-new-privileges",
            "--cap-drop",
            "ALL",
            "--read-only",
            "--tmpfs",
            "/tmp:size=64m",
            "-v",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        args.push(format!("{confined_root}:/workspace:rw"));
        args.push("-w".to_owned());
        args.push("/workspace".to_owned());

        // Shadow .git with a read-only mount to prevent host git-hook escape.
        if let Some(git) = git_dir {
            args.push("-v".to_owned());
            args.push(format!("{git}:/workspace/.git:ro"));
        }

        args.push("--".to_owned());
        args.push(image.to_owned());
        args.push("sh".to_owned());
        args.push("-c".to_owned());
        args.push(cmd.to_owned());
        args
    }
}

#[async_trait]
impl SandboxBackend for DockerBackend {
    fn name(&self) -> &'static str {
        "docker"
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
            return Err("docker sandbox not available".into());
        }

        let (root, git) = resolve_confined_root(confined_root)?;
        let root_str = root
            .to_str()
            .ok_or("confined root contains non-UTF-8 bytes")?;
        let git_str = match git.as_ref() {
            Some(g) => Some(
                g.to_str()
                    .ok_or("git dir contains non-UTF-8 bytes")?
                    .to_owned(),
            ),
            None => None,
        };

        let args = Self::build_run_args(&self.image, root_str, git_str.as_deref(), cmd, policy);

        match tokio::time::timeout(
            std::time::Duration::from_secs(EXEC_TIMEOUT_SECS),
            tokio::process::Command::new("docker").args(&args).output(),
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

    #[test]
    fn network_arg_maps_policies() {
        assert_eq!(DockerBackend::network_arg(NetworkPolicy::None), "none");
        assert_eq!(
            DockerBackend::network_arg(NetworkPolicy::Allowlist),
            "bridge"
        );
        assert_eq!(DockerBackend::network_arg(NetworkPolicy::Open), "bridge");
    }

    #[tokio::test]
    async fn exec_unavailable_returns_err() {
        let b = DockerBackend {
            available: false,
            image: String::new(),
        };
        assert!(b
            .exec("ls", Path::new("/tmp"), NetworkPolicy::None)
            .await
            .is_err());
    }

    // ── 4.1 structural read floor: only the confined root is mounted ──────────

    #[test]
    fn docker_run_args_do_not_mount_host_home() {
        // Assemble the run args for a confined root with no .git.
        let args = DockerBackend::build_run_args(
            "smedja-sandbox:latest",
            "/work/root",
            None,
            "echo hi",
            NetworkPolicy::Open,
        );

        // Exactly one read-write bind mount: the confined root → /workspace.
        let mounts: Vec<&String> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(flag, _)| flag.as_str() == "-v")
            .map(|(_, val)| val)
            .collect();
        assert_eq!(
            mounts,
            vec![&"/work/root:/workspace:rw".to_owned()],
            "only the confined root is mounted; got: {mounts:?}"
        );

        // No bind mount references the host home dir (structural read floor).
        if let Ok(home) = std::env::var("HOME") {
            assert!(
                !args.iter().any(|a| a.contains(&home)),
                "no host-home path may be mounted; got: {args:?}"
            );
        }
    }

    // ── 6.2 --network none under NetworkPolicy::None ──────────────────────────

    #[test]
    fn docker_run_args_isolate_network_under_none() {
        let args = DockerBackend::build_run_args(
            "img",
            "/work/root",
            None,
            "echo hi",
            NetworkPolicy::None,
        );
        let net_idx = args.iter().position(|a| a == "--network").unwrap();
        assert_eq!(
            args[net_idx + 1],
            "none",
            "NetworkPolicy::None must produce --network none; got: {args:?}"
        );
    }
}
