//! Docker sandbox executor for tool isolation.
//!
//! When `SMEDJA_TOOL_SANDBOX=docker` is set and the `docker` binary is
//! reachable, write and execute tools run inside an ephemeral Alpine
//! container with the workspace bind-mounted and no network access.
//! Read-only tools bypass the sandbox entirely.

use std::path::Path;

use which::which;

/// Tools exempt from sandboxing (read-only; no side-effects).
const EXEMPT_TOOLS: &[&str] = &["read_file", "list_files", "graph_query"];

/// Executes bash commands (and write-class tools) inside a Docker container.
pub struct SandboxExecutor {
    /// `true` if Docker is reachable and the sandbox image exists.
    pub available: bool,
    /// Digest of the smedja-sandbox image.
    image: String,
}

impl SandboxExecutor {
    /// Creates a new executor and checks Docker availability.
    ///
    /// Sets `available = false` if the `docker` binary is absent or
    /// `SMEDJA_TOOL_SANDBOX` is not set to `"docker"`.
    ///
    /// # Note
    ///
    /// The image-inspect step uses a blocking `std::process::Command`. This is
    /// intentional: `new()` is called once at daemon startup before the Tokio
    /// runtime is accepting work, so a brief blocking call here is acceptable
    /// and avoids the complexity of an `async fn new()`.
    pub fn new() -> Self {
        let enabled = std::env::var("SMEDJA_TOOL_SANDBOX").is_ok_and(|v| v == "docker");

        if !enabled {
            return Self {
                available: false,
                image: String::new(),
            };
        }

        let docker_ok = which("docker").is_ok();
        if !docker_ok {
            tracing::warn!("SMEDJA_TOOL_SANDBOX=docker but docker binary not found");
            return Self {
                available: false,
                image: String::new(),
            };
        }

        // Verify the image exists. Allow operator to pin to a digest via env var.
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

    /// Returns `true` if `tool_name` is exempt from sandboxing.
    #[must_use]
    pub fn is_exempt(tool_name: &str) -> bool {
        EXEMPT_TOOLS.contains(&tool_name)
    }

    /// Executes `cmd` inside the sandbox container.
    ///
    /// Mounts `workspace` at `/workspace` read-write; no network access.
    /// Resource limits: 0.5 CPU, 256 MiB RAM, 64 PIDs, 30-second stop timeout.
    /// Security: dropped capabilities, read-only root filesystem, `/tmp` tmpfs
    /// (64 MiB), no-new-privileges, and the `.git` directory (if present)
    /// bind-mounted read-only on top of the workspace mount to prevent git-hook
    /// escape.
    ///
    /// Returns the combined stdout of the container.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the workspace path is invalid, Docker is unavailable,
    /// the command times out after 30 seconds, or the container exits non-zero.
    pub async fn exec(&self, cmd: &str, workspace: &Path) -> Result<String, String> {
        if !self.available {
            return Err("sandbox not available".into());
        }

        // Canonicalise and optionally validate against an allowed root.
        let workspace = workspace
            .canonicalize()
            .map_err(|e| format!("invalid workspace: {e}"))?;
        let allowed_root = std::env::var("SMEDJA_WORKSPACE_ROOT").map_or_else(
            |_| {
                std::env::var("HOME")
                    .map_or_else(|_| std::path::PathBuf::from("/"), std::path::PathBuf::from)
            },
            std::path::PathBuf::from,
        );
        if std::env::var("SMEDJA_WORKSPACE_ROOT").is_ok() && !workspace.starts_with(&allowed_root) {
            return Err(format!(
                "workspace {} is outside allowed root {}",
                workspace.display(),
                allowed_root.display()
            ));
        }

        let workspace_str = workspace
            .to_str()
            .ok_or("workspace path contains non-UTF-8 bytes")?;
        let workspace_str_rw = format!("{workspace_str}:/workspace:rw");

        let git_dir = workspace.join(".git");

        // Build the args vec dynamically so we can conditionally add the
        // read-only .git override mount.
        let mut args: Vec<&str> = vec![
            "run",
            "--rm",
            "--network",
            "none",
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
            &workspace_str_rw,
            "-w",
            "/workspace",
        ];

        // Shadow .git with a read-only mount if the directory exists.
        // This prevents an agent writing hooks that execute on the host.
        let git_vol;
        if git_dir.exists() {
            git_vol = format!("{}:/workspace/.git:ro", git_dir.display());
            args.push("-v");
            args.push(&git_vol);
        }

        args.extend_from_slice(&["--", &self.image, "sh", "-c", cmd]);

        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::process::Command::new("docker").args(&args).output(),
        )
        .await
        {
            Err(_) => Err("sandbox: command timed out after 30 seconds".to_owned()),
            Ok(Err(e)) => Err(e.to_string()),
            Ok(Ok(out)) if out.status.success() => {
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            }
            Ok(Ok(out)) => Err(String::from_utf8_lossy(&out.stderr).into_owned()),
        }
    }
}

impl Default for SandboxExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_when_env_unset_is_unavailable() {
        // SMEDJA_TOOL_SANDBOX not set in test env → available = false.
        // (Don't set it in tests to avoid requiring Docker in CI.)
        let ex = SandboxExecutor::new();
        assert!(!ex.available);
    }

    #[test]
    fn read_file_is_exempt() {
        assert!(SandboxExecutor::is_exempt("read_file"));
    }

    #[test]
    fn bash_is_not_exempt() {
        assert!(!SandboxExecutor::is_exempt("bash"));
    }

    #[test]
    fn mcp_call_is_not_exempt() {
        // mcp_call does not exist in the codebase; it must not be exempt.
        assert!(!SandboxExecutor::is_exempt("mcp_call"));
    }

    #[tokio::test]
    async fn exec_unavailable_returns_err() {
        let ex = SandboxExecutor {
            available: false,
            image: String::new(),
        };
        assert!(ex.exec("ls", std::path::Path::new("/tmp")).await.is_err());
    }

    #[tokio::test]
    async fn exec_rejects_workspace_outside_allowed_root() {
        // Set a tight allowed root so /tmp is outside it.
        std::env::set_var("SMEDJA_WORKSPACE_ROOT", "/nonexistent-root-xyz");
        let ex = SandboxExecutor {
            available: true,
            image: "smedja-sandbox:latest".to_owned(),
        };
        let result = ex.exec("ls", std::path::Path::new("/tmp")).await;
        std::env::remove_var("SMEDJA_WORKSPACE_ROOT");
        // /tmp canonicalises to /tmp; /nonexistent-root-xyz doesn't contain it.
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(
            msg.contains("outside allowed root") || msg.contains("invalid workspace"),
            "unexpected error: {msg}"
        );
    }
}
