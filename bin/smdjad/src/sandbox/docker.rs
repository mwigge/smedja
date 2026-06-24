//! Docker isolation backend.
//!
//! Runs the command inside an ephemeral container with the confined root
//! bind-mounted read-write, `.git` shadowed read-only, a dropped capability
//! set, a read-only root filesystem, and a `/tmp` tmpfs. The network is
//! configured per [`NetworkPolicy`].

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
        let root_rw = format!("{root_str}:/workspace:rw");
        let network = Self::network_arg(policy);

        let mut args: Vec<&str> = vec![
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
            &root_rw,
            "-w",
            "/workspace",
        ];

        // Shadow .git with a read-only mount to prevent host git-hook escape.
        let git_vol;
        if let Some(git_dir) = git {
            git_vol = format!("{}:/workspace/.git:ro", git_dir.display());
            args.push("-v");
            args.push(&git_vol);
        }

        args.extend_from_slice(&["--", &self.image, "sh", "-c", cmd]);

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
}
