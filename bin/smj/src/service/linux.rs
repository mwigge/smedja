//! Linux systemd --user service management for smdjad.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use super::ServiceAction;

const SERVICE_NAME: &str = "smdjad";

pub fn dispatch(action: &ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install => install(),
        ServiceAction::Uninstall => uninstall(),
        ServiceAction::Status => status(),
        ServiceAction::Logs => logs(),
        ServiceAction::Restart => restart(),
    }
}

fn unit_path() -> PathBuf {
    let config_home = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        std::env::var("HOME").map_or_else(|_| ".config".to_owned(), |h| format!("{h}/.config"))
    });
    PathBuf::from(config_home)
        .join("systemd")
        .join("user")
        .join(format!("{SERVICE_NAME}.service"))
}

fn write_unit(smdjad_bin: &Path) -> Result<()> {
    let unit = unit_path();
    let otlp_endpoint = std::env::var("SMEDJA_OTLP_ENDPOINT").ok();
    write_unit_inner(smdjad_bin, &unit, otlp_endpoint.as_deref())
}

fn write_unit_inner(smdjad_bin: &Path, unit: &Path, otlp_endpoint: Option<&str>) -> Result<()> {
    let parent = unit.parent().expect("unit path always has a parent");
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;

    // Use the systemd %U specifier rather than baking the installer's literal
    // XDG_RUNTIME_DIR. %U resolves to the running user's UID at unit-start
    // time, so /run/user/%U stays correct even if the UID differs from the one
    // that installed the unit. This mirrors assets/smdjad.service.
    let mut env_lines = String::from("Environment=XDG_RUNTIME_DIR=/run/user/%U\n");
    if let Some(ep) = otlp_endpoint {
        let _ = writeln!(env_lines, "Environment=\"SMEDJA_OTLP_ENDPOINT={ep}\"");
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
    let unit = unit_path();
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
    anyhow::ensure!(status.success(), "systemctl restart {SERVICE_NAME} failed");
    println!("smdjad restarted");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_path_ends_with_correct_filename() {
        let path = unit_path();
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

        write_unit_inner(&fake_bin, &unit_dest, None).unwrap();

        let content = std::fs::read_to_string(&unit_dest).unwrap();
        assert!(content.contains(fake_bin.to_str().unwrap()));
        assert!(content.contains("smdjad agent daemon"));
        assert!(content.contains("Restart=on-failure"));
    }

    #[test]
    fn write_unit_inner_uses_runtime_dir_specifier() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join("smdjad");
        std::fs::write(&fake_bin, b"").unwrap();
        let unit_dest = dir.path().join("smdjad.service");

        write_unit_inner(&fake_bin, &unit_dest, None).unwrap();

        let content = std::fs::read_to_string(&unit_dest).unwrap();
        // The %U specifier keeps the runtime dir correct across UID changes;
        // a literal /run/user/<uid> must never be baked in.
        assert!(content.contains("XDG_RUNTIME_DIR=/run/user/%U"));
    }

    #[test]
    fn write_unit_inner_includes_otlp_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join("smdjad");
        std::fs::write(&fake_bin, b"").unwrap();
        let unit_dest = dir.path().join("smdjad.service");

        write_unit_inner(&fake_bin, &unit_dest, Some("http://localhost:4317")).unwrap();

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

        write_unit_inner(&fake_bin, &unit_dest, None).unwrap();

        let content = std::fs::read_to_string(&unit_dest).unwrap();
        assert!(!content.contains("SMEDJA_OTLP_ENDPOINT"));
    }
}
