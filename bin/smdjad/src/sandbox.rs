//! Docker sandbox executor for tool isolation.
//!
//! When `SMEDJA_TOOL_SANDBOX=docker` is set and the `docker` binary is
//! reachable, write and execute tools run inside an ephemeral Alpine
//! container with the workspace bind-mounted and no network access.
//! Read-only tools bypass the sandbox entirely.

use std::path::Path;

use which::which;

/// Tools exempt from sandboxing (read-only; no side-effects).
const EXEMPT_TOOLS: &[&str] = &["read_file", "list_files", "graph_query", "mcp_call"];

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

        // Verify the image exists.
        let image = "smedja-sandbox:latest".to_owned();
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
    /// Returns the combined stdout of the container.
    ///
    /// # Errors
    ///
    /// Returns `Err` if Docker is unavailable or the container exits non-zero.
    pub fn exec(&self, cmd: &str, workspace: &Path) -> Result<String, String> {
        if !self.available {
            return Err("sandbox not available".into());
        }
        let workspace_str = workspace.to_str().ok_or("workspace path invalid")?;
        let out = std::process::Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network",
                "none",
                "-v",
                &format!("{workspace_str}:/workspace:rw"),
                "-w",
                "/workspace",
                &self.image,
                "bash",
                "-c",
                cmd,
            ])
            .output()
            .map_err(|e| e.to_string())?;

        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
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
    fn exec_unavailable_returns_err() {
        let ex = SandboxExecutor {
            available: false,
            image: String::new(),
        };
        assert!(ex.exec("ls", std::path::Path::new("/tmp")).is_err());
    }
}
