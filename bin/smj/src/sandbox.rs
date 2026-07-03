//! `smj sandbox` — Docker sandbox management and backend status reporting.

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum SandboxCmd {
    /// Build the smedja-sandbox Docker image
    Build,
    /// Report the selected backend, its availability, the network policy, and
    /// the fallback mode
    Status,
}

/// Dispatches a `smj sandbox` subcommand.
pub(crate) fn run(action: SandboxCmd) -> Result<()> {
    match action {
        SandboxCmd::Build => {
            println!("Building smedja-sandbox:latest...");
            let status = std::process::Command::new("docker")
                .args(["build", "-t", "smedja-sandbox:latest", "scripts/sandbox/"])
                .status()
                .map_err(|e| anyhow::anyhow!("docker not found: {e}"))?;
            if status.success() {
                println!("Image built successfully.");
            } else {
                anyhow::bail!("docker build failed");
            }
        }
        SandboxCmd::Status => {
            let status = SandboxStatus::detect();
            println!("Sandbox backend: {}", status.backend);
            println!(
                "Available:       {}",
                if status.available { "yes" } else { "no" }
            );
            println!("Network policy:  {}", status.network_policy);
            println!("Fallback mode:   {}", status.mode);
        }
    }
    Ok(())
}

/// Operator-facing view of the active sandbox configuration.
///
/// Mirrors the daemon's backend-selection precedence (Docker when opted in and
/// reachable → the current platform's OS-native backend → none) and reads the
/// same environment contract (`SMEDJA_SANDBOX_MODE`, `SMEDJA_SANDBOX_NETWORK`,
/// legacy `SMEDJA_TOOL_SANDBOX=docker`) so `smj sandbox status` reports what the
/// daemon would select.
struct SandboxStatus {
    backend: &'static str,
    available: bool,
    network_policy: &'static str,
    mode: &'static str,
}

impl SandboxStatus {
    fn detect() -> Self {
        let legacy_docker = std::env::var("SMEDJA_TOOL_SANDBOX").is_ok_and(|v| v == "docker");
        let mode = if legacy_docker {
            "auto"
        } else {
            Self::mode_from_env()
        };
        let network_policy = Self::network_from_env();

        if mode == "off" {
            return Self {
                backend: "none",
                available: false,
                network_policy,
                mode,
            };
        }

        let docker_opt_in = legacy_docker || which::which("docker").is_ok();
        let docker_avail = docker_opt_in && Self::docker_image_ok();
        let (backend, available) = if docker_avail {
            ("docker", true)
        } else if cfg!(target_os = "macos") {
            ("seatbelt", which::which("sandbox-exec").is_ok())
        } else if cfg!(target_os = "linux") {
            // Landlock availability is a kernel property the CLI cannot probe
            // without the daemon; report the native backend name and defer the
            // definitive availability to the daemon's own detection.
            ("landlock", true)
        } else {
            ("none", false)
        };

        Self {
            backend,
            available,
            network_policy,
            mode,
        }
    }

    fn mode_from_env() -> &'static str {
        Self::mode_from_value(&std::env::var("SMEDJA_SANDBOX_MODE").unwrap_or_default())
    }

    fn mode_from_value(value: &str) -> &'static str {
        match value.trim().to_ascii_lowercase().as_str() {
            "required" => "required",
            "off" => "off",
            _ => "auto",
        }
    }

    fn network_from_env() -> &'static str {
        Self::network_from_value(&std::env::var("SMEDJA_SANDBOX_NETWORK").unwrap_or_default())
    }

    fn network_from_value(value: &str) -> &'static str {
        match value.trim().to_ascii_lowercase().as_str() {
            "allowlist" => "allowlist",
            "open" => "open",
            _ => "none",
        }
    }

    fn docker_image_ok() -> bool {
        if which::which("docker").is_err() {
            return false;
        }
        let image = std::env::var("SMEDJA_SANDBOX_IMAGE")
            .unwrap_or_else(|_| "smedja-sandbox:latest".to_owned());
        std::process::Command::new("docker")
            .args(["image", "inspect", "--format", "{{.Id}}", &image])
            .output()
            .is_ok_and(|o| o.status.success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Cmd};
    use clap::Parser as _;

    #[test]
    fn sandbox_status_parses_subcommand() {
        let cli =
            Cli::try_parse_from(["smj", "sandbox", "status"]).expect("sandbox status must parse");
        assert!(matches!(
            cli.command,
            Cmd::Sandbox {
                action: SandboxCmd::Status
            }
        ));
    }

    #[test]
    fn sandbox_status_reports_backend_and_policy() {
        // Mode parsing covers the fallback-mode column.
        assert_eq!(SandboxStatus::mode_from_value("required"), "required");
        assert_eq!(SandboxStatus::mode_from_value("off"), "off");
        assert_eq!(SandboxStatus::mode_from_value(""), "auto");
        assert_eq!(SandboxStatus::mode_from_value("nonsense"), "auto");

        // Network policy parsing covers the network-policy column.
        assert_eq!(SandboxStatus::network_from_value("allowlist"), "allowlist");
        assert_eq!(SandboxStatus::network_from_value("open"), "open");
        assert_eq!(SandboxStatus::network_from_value(""), "none");

        // The resolved status reports a backend name, an availability flag, the
        // network policy, and the fallback mode for the current host.
        let status = SandboxStatus::detect();
        assert!(
            matches!(status.backend, "docker" | "seatbelt" | "landlock" | "none"),
            "backend must be one of the known names; got {}",
            status.backend
        );
        assert!(matches!(status.mode, "auto" | "required" | "off"));
        assert!(matches!(
            status.network_policy,
            "none" | "allowlist" | "open"
        ));
        // `available` is a bool by construction; assert it is consistent with
        // the "none" backend never being available.
        if status.backend == "none" {
            assert!(!status.available);
        }
    }
}
