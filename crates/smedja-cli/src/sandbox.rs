use super::*;

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
