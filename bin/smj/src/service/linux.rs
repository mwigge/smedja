//! Linux systemd --user service management for smdjad.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use super::ServiceAction;

const SERVICE_NAME: &str = "smdjad";

pub fn dispatch(action: ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install => install(),
        ServiceAction::Uninstall => uninstall(),
        ServiceAction::Status => status(),
        ServiceAction::Logs => logs(),
        ServiceAction::Restart => restart(),
    }
}

fn unit_path() -> Result<PathBuf> {
    let config_home = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        std::env::var("HOME")
            .map(|h| format!("{h}/.config"))
            .unwrap_or_else(|_| ".config".to_owned())
    });
    Ok(PathBuf::from(config_home)
        .join("systemd")
        .join("user")
        .join(format!("{SERVICE_NAME}.service")))
}

fn write_unit(smdjad_bin: &Path) -> Result<()> {
    let unit = unit_path()?;
    let xdg_runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let otlp_endpoint = std::env::var("SMEDJA_OTLP_ENDPOINT").ok();
    write_unit_inner(smdjad_bin, &unit, &xdg_runtime_dir, otlp_endpoint.as_deref())
}

fn write_unit_inner(
    smdjad_bin: &Path,
    unit: &Path,
    xdg_runtime_dir: &str,
    otlp_endpoint: Option<&str>,
) -> Result<()> {
    let parent = unit.parent().expect("unit path always has a parent");
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;

    let mut env_lines = format!("Environment=\"XDG_RUNTIME_DIR={xdg_runtime_dir}\"\n");
    if let Some(ep) = otlp_endpoint {
        env_lines.push_str(&format!("Environment=\"SMEDJA_OTLP_ENDPOINT={ep}\"\n"));
    }

    let content = format!(
        "[Unit]\nDescription=smedja agent daemon\nAfter=default.target\n\n[Service]\nExecStart={bin}\nRestart=on-failure\nRestartSec=3s\n{env_lines}\n[Install]\nWantedBy=default.target\n",
        bin = smdjad_bin.display(),
    );

    std::fs::write(unit, content)
        .with_context(|| format!("cannot write unit file to {}", unit.display()))?;
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<std::process::ExitStatus> {
    std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("systemctl --user failed")
}

pub fn install() -> Result<()> {
    let smdjad_bin = which::which("smdjad").context(
        "smdjad not found on PATH — build and install it before running `smj service install`",
    )?;
    write_unit(&smdjad_bin)?;
    let status = systemctl(&["daemon-reload"])?;
    anyhow::ensure!(status.success(), "systemctl daemon-reload failed");
    let status = systemctl(&["enable", "--now", SERVICE_NAME])?;
    anyhow::ensure!(
        status.success(),
        "systemctl enable --now {SERVICE_NAME} failed"
    );
    println!("smdjad service installed and started");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let _ = systemctl(&["disable", "--now", SERVICE_NAME]);
    let unit = unit_path()?;
    if unit.exists() {
        std::fs::remove_file(&unit)
            .with_context(|| format!("cannot remove unit file {}", unit.display()))?;
    }
    let _ = systemctl(&["daemon-reload"]);
    println!("smdjad service removed");
    Ok(())
}

pub fn status() -> Result<()> {
    systemctl(&["status", SERVICE_NAME])?;
    Ok(())
}

pub fn logs() -> Result<()> {
    let status = std::process::Command::new("journalctl")
        .args(["--user", "-u", SERVICE_NAME, "-n", "50", "--no-pager"])
        .status()
        .context("journalctl failed")?;
    anyhow::ensure!(status.success(), "journalctl exited with status {status}");
    Ok(())
}

pub fn restart() -> Result<()> {
    let status = systemctl(&["restart", SERVICE_NAME])?;
    anyhow::ensure!(
        status.success(),
        "systemctl restart {SERVICE_NAME} failed"
    );
    println!("smdjad restarted");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_path_ends_with_correct_filename() {
        let path = unit_path().unwrap();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "smdjad.service"
        );
    }

    #[test]
    fn write_unit_inner_contains_binary_path() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join("smdjad");
        std::fs::write(&fake_bin, b"").unwrap();
        let unit_dest = dir.path().join("smdjad.service");

        write_unit_inner(&fake_bin, &unit_dest, "/run/user/1000", None).unwrap();

        let content = std::fs::read_to_string(&unit_dest).unwrap();
        assert!(content.contains(fake_bin.to_str().unwrap()));
        assert!(content.contains("smdjad agent daemon"));
        assert!(content.contains("Restart=on-failure"));
    }

    #[test]
    fn write_unit_inner_includes_otlp_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join("smdjad");
        std::fs::write(&fake_bin, b"").unwrap();
        let unit_dest = dir.path().join("smdjad.service");

        write_unit_inner(
            &fake_bin,
            &unit_dest,
            "/run/user/1000",
            Some("http://localhost:4317"),
        )
        .unwrap();

        let content = std::fs::read_to_string(&unit_dest).unwrap();
        assert!(content.contains("SMEDJA_OTLP_ENDPOINT"));
        assert!(content.contains("http://localhost:4317"));
    }

    #[test]
    fn write_unit_inner_excludes_otlp_when_not_set() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join("smdjad");
        std::fs::write(&fake_bin, b"").unwrap();
        let unit_dest = dir.path().join("smdjad.service");

        write_unit_inner(&fake_bin, &unit_dest, "/run/user/1000", None).unwrap();

        let content = std::fs::read_to_string(&unit_dest).unwrap();
        assert!(!content.contains("SMEDJA_OTLP_ENDPOINT"));
    }
}
