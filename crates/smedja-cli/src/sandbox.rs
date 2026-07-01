use super::*;

/// Operator-facing view of the active sandbox configuration.
///
/// Mirrors the daemon's backend-selection precedence (Docker when opted in and
/// reachable -> the current platform's OS-native backend -> none) and reads the
/// same environment contract (`SMEDJA_SANDBOX_MODE`, `SMEDJA_SANDBOX_NETWORK`,
/// legacy `SMEDJA_TOOL_SANDBOX=docker`) so `smj sandbox status` reports what the
/// daemon would select.
pub(crate) struct SandboxStatus {
    pub(crate) backend: &'static str,
    pub(crate) available: bool,
    pub(crate) network_policy: &'static str,
    pub(crate) mode: &'static str,
}

impl SandboxStatus {
    pub(crate) fn detect() -> Self {
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

    pub(crate) fn mode_from_value(value: &str) -> &'static str {
        match value.trim().to_ascii_lowercase().as_str() {
            "required" => "required",
            "off" => "off",
            _ => "auto",
        }
    }

    fn network_from_env() -> &'static str {
        Self::network_from_value(&std::env::var("SMEDJA_SANDBOX_NETWORK").unwrap_or_default())
    }

    pub(crate) fn network_from_value(value: &str) -> &'static str {
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

pub(crate) fn dispatch_sandbox(action: SandboxCmd) -> Result<()> {
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
