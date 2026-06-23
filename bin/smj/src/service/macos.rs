//! macOS launchd service management for smdjad.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use super::ServiceAction;

const LABEL: &str = "nu.wigge.smdjad";

pub fn dispatch(action: ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install => install(),
        ServiceAction::Uninstall => uninstall(),
        ServiceAction::Status => status(),
        ServiceAction::Logs => logs(),
        ServiceAction::Restart => restart(),
    }
}

fn plist_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn log_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("Library").join("Logs").join("smdjad"))
}

fn current_uid() -> Result<String> {
    let out = std::process::Command::new("id")
        .arg("-u")
        .output()
        .context("failed to run `id -u`")?;
    Ok(String::from_utf8(out.stdout)
        .context("id -u output is not UTF-8")?
        .trim()
        .to_owned())
}

fn write_plist(smdjad_bin: &Path) -> Result<()> {
    let plist = plist_path()?;
    let log = log_dir()?;
    std::fs::create_dir_all(&log)
        .with_context(|| format!("cannot create log dir {}", log.display()))?;

    let xdg_runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let otlp_endpoint = std::env::var("SMEDJA_OTLP_ENDPOINT").ok();
    write_plist_inner(smdjad_bin, &plist, &log, &xdg_runtime_dir, otlp_endpoint.as_deref())
}

fn write_plist_inner(
    smdjad_bin: &Path,
    plist: &Path,
    log_dir: &Path,
    xdg_runtime_dir: &str,
    otlp_endpoint: Option<&str>,
) -> Result<()> {
    std::fs::create_dir_all(log_dir)
        .with_context(|| format!("cannot create log dir {}", log_dir.display()))?;
    let log_path = log_dir.join("smdjad.log");

    let mut env_entries =
        format!("        <key>XDG_RUNTIME_DIR</key> <string>{xdg_runtime_dir}</string>\n");
    if let Some(ep) = otlp_endpoint {
        env_entries.push_str(&format!(
            "        <key>SMEDJA_OTLP_ENDPOINT</key> <string>{ep}</string>\n"
        ));
    }

    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>              <string>{LABEL}</string>
    <key>ProgramArguments</key>   <array><string>{bin}</string></array>
    <key>KeepAlive</key>          <true/>
    <key>RunAtLoad</key>          <true/>
    <key>StandardOutPath</key>    <string>{log}</string>
    <key>StandardErrorPath</key>  <string>{log}</string>
    <key>EnvironmentVariables</key>
    <dict>
{env_entries}    </dict>
</dict>
</plist>
"#,
        bin = smdjad_bin.display(),
        log = log_path.display(),
    );

    std::fs::write(plist, content)
        .with_context(|| format!("cannot write plist to {}", plist.display()))?;
    Ok(())
}

pub fn install() -> Result<()> {
    let smdjad_bin = which::which("smdjad").context(
        "smdjad not found on PATH — build and install it before running `smj service install`",
    )?;
    write_plist(&smdjad_bin)?;
    let plist = plist_path()?;
    let uid = current_uid()?;
    let status = std::process::Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()])
        .status()
        .context("launchctl bootstrap failed")?;
    anyhow::ensure!(
        status.success(),
        "launchctl bootstrap exited with status {status}"
    );
    println!("smdjad service installed and started (label: {LABEL})");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let uid = current_uid()?;
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{LABEL}")])
        .status();
    let plist = plist_path()?;
    if plist.exists() {
        std::fs::remove_file(&plist)
            .with_context(|| format!("cannot remove plist {}", plist.display()))?;
    }
    println!("smdjad service removed");
    Ok(())
}

pub fn status() -> Result<()> {
    let uid = current_uid()?;
    let output = std::process::Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{LABEL}")])
        .output()
        .context("launchctl print failed")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        print!("{stdout}");
    } else {
        print!("{stderr}");
        println!("(smdjad service may not be installed — run `smj service install`)");
    }
    Ok(())
}

pub fn logs() -> Result<()> {
    let log = log_dir()?.join("smdjad.log");
    if !log.exists() {
        println!("No log file found at {}", log.display());
        println!("(service may not be installed or has not produced output yet)");
        return Ok(());
    }
    let status = std::process::Command::new("tail")
        .args(["-n", "50", "-f", &log.to_string_lossy()])
        .status()
        .context("tail failed")?;
    anyhow::ensure!(status.success(), "tail exited with status {status}");
    Ok(())
}

pub fn restart() -> Result<()> {
    let uid = current_uid()?;
    let status = std::process::Command::new("launchctl")
        .args(["kickstart", "-k", &format!("gui/{uid}/{LABEL}")])
        .status()
        .context("launchctl kickstart failed")?;
    anyhow::ensure!(
        status.success(),
        "launchctl kickstart exited with status {status}"
    );
    println!("smdjad restarted");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_path_ends_with_correct_filename() {
        let path = plist_path().unwrap();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "nu.wigge.smdjad.plist"
        );
    }

    #[test]
    fn write_plist_inner_contains_binary_path() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join("smdjad");
        std::fs::write(&fake_bin, b"").unwrap();
        let log_dir = dir.path().join("logs");
        let plist_dest = dir.path().join("smdjad.plist");

        write_plist_inner(&fake_bin, &plist_dest, &log_dir, "/run/user/1000", None).unwrap();

        let content = std::fs::read_to_string(&plist_dest).unwrap();
        assert!(content.contains(fake_bin.to_str().unwrap()));
        assert!(content.contains("nu.wigge.smdjad"));
        assert!(content.contains("<true/>"), "KeepAlive must be true");
    }

    #[test]
    fn write_plist_inner_includes_otlp_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join("smdjad");
        std::fs::write(&fake_bin, b"").unwrap();
        let plist_dest = dir.path().join("smdjad.plist");

        write_plist_inner(
            &fake_bin,
            &plist_dest,
            &dir.path().join("logs"),
            "/run/user/1000",
            Some("http://localhost:4317"),
        )
        .unwrap();

        let content = std::fs::read_to_string(&plist_dest).unwrap();
        assert!(content.contains("SMEDJA_OTLP_ENDPOINT"));
        assert!(content.contains("http://localhost:4317"));
    }

    #[test]
    fn write_plist_inner_excludes_otlp_when_not_set() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join("smdjad");
        std::fs::write(&fake_bin, b"").unwrap();
        let plist_dest = dir.path().join("smdjad.plist");

        write_plist_inner(
            &fake_bin,
            &plist_dest,
            &dir.path().join("logs"),
            "/run/user/1000",
            None,
        )
        .unwrap();

        let content = std::fs::read_to_string(&plist_dest).unwrap();
        assert!(!content.contains("SMEDJA_OTLP_ENDPOINT"));
    }
}
