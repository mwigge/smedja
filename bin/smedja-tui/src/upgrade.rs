/// Current binary version, baked in at compile time.
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Fetches the latest release tag from the GitHub API.
///
/// Returns `Some("v0.15.1")` on success, `None` on any network or parse
/// failure.  Uses `curl` as an external subprocess so we don't need a full
/// HTTP client dependency.
pub(crate) async fn fetch_latest_version() -> Option<String> {
    let out = tokio::process::Command::new("curl")
        .args([
            "-sf",
            "--max-time",
            "10",
            "-H",
            "Accept: application/vnd.github.v3+json",
            "-H",
            &format!("User-Agent: smedja-tui/{VERSION}"),
            "https://api.github.com/repos/mwigge/smedja/releases/latest",
        ])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    body.get("tag_name")?.as_str().map(str::to_owned)
}

/// Returns `true` when `latest` (e.g. `"v0.16.0"`) is strictly greater than
/// `current` (e.g. `"0.15.0"`).  Leading `v` is stripped before comparison.
pub(crate) fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Option<(u64, u64, u64)> {
        let v = v.trim_start_matches('v');
        let p: Vec<u64> = v.split('.').filter_map(|s| s.parse().ok()).collect();
        if p.len() == 3 {
            Some((p[0], p[1], p[2]))
        } else {
            None
        }
    };
    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn release_os() -> Option<&'static str> {
    if cfg!(target_os = "linux") {
        Some("linux")
    } else if cfg!(target_os = "macos") {
        Some("darwin")
    } else {
        None
    }
}

fn release_arch() -> Option<&'static str> {
    if cfg!(target_arch = "x86_64") {
        Some("x86_64")
    } else if cfg!(target_arch = "aarch64") {
        Some("aarch64")
    } else {
        None
    }
}

/// Downloads the latest release tarball and installs the binaries alongside
/// the currently-running executable.
///
/// Returns a human-readable outcome string (success or error details) so the
/// caller can push it straight into the panel.
pub(crate) async fn run_upgrade(latest_tag: &str) -> String {
    let Some(os) = release_os() else {
        return "upgrade failed: unsupported operating system".into();
    };
    let Some(arch) = release_arch() else {
        return "upgrade failed: unsupported CPU architecture".into();
    };
    let artifact = format!("smedja-{os}-{arch}");
    let url = format!(
        "https://github.com/mwigge/smedja/releases/download/{latest_tag}/{artifact}.tar.gz"
    );

    // Resolve the directory that contains the currently running smedja-tui
    // binary; new binaries will be placed alongside it.
    let Some(install_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
    else {
        return "upgrade failed: could not determine install directory".into();
    };

    let tmp = std::env::temp_dir().join("smedja-upgrade");
    let _ = std::fs::remove_dir_all(&tmp);
    if std::fs::create_dir_all(&tmp).is_err() {
        return "upgrade failed: could not create temp directory".into();
    }
    let tarball = tmp.join("release.tar.gz");

    // Download
    let dl = tokio::process::Command::new("curl")
        .args(["-sfL", "--max-time", "120", &url, "-o"])
        .arg(&tarball)
        .status()
        .await;
    if !matches!(dl, Ok(s) if s.success()) {
        let _ = std::fs::remove_dir_all(&tmp);
        return format!("upgrade failed: could not download {url}");
    }

    // Extract
    let ex = tokio::process::Command::new("tar")
        .args(["-xzf"])
        .arg(&tarball)
        .arg("-C")
        .arg(&tmp)
        .status()
        .await;
    if !matches!(ex, Ok(s) if s.success()) {
        let _ = std::fs::remove_dir_all(&tmp);
        return "upgrade failed: extraction error".into();
    }

    // Install each binary with mv (atomic) falling back to copy.
    let src_dir = tmp.join(&artifact);
    let bins = ["smedja", "smedja-tui", "smdjad", "smj"];
    let mut installed = Vec::new();
    let mut failed: Vec<String> = Vec::new();

    for bin in bins {
        let src = src_dir.join(bin);
        let dst = install_dir.join(bin);
        if !src.exists() {
            continue;
        }
        // mv is atomic on the same filesystem; fall back to copy for cross-fs
        let ok = std::fs::rename(&src, &dst)
            .or_else(|_| std::fs::copy(&src, &dst).map(|_| ()))
            .is_ok();
        if ok {
            installed.push(bin);
        } else {
            failed.push(bin.into());
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    if installed.is_empty() && failed.is_empty() {
        return "upgrade failed: release artifact did not contain expected binaries".into();
    }

    #[cfg(target_os = "macos")]
    for bin in bins {
        let _ = tokio::process::Command::new("xattr")
            .args(["-dr", "com.apple.quarantine"])
            .arg(install_dir.join(bin))
            .status()
            .await;
    }

    let smdjad_restart = restart_smdjad_after_upgrade().await;

    let mut msg = if failed.is_empty() {
        format!(
            "upgraded to {latest_tag} ({})\nrestart smedja to use the new binary",
            installed.join(", ")
        )
    } else {
        format!(
            "partial upgrade to {latest_tag}\n  ok: {}\n  failed: {}",
            installed.join(", "),
            failed.join(", ")
        )
    };
    if let Some(restart_method) = smdjad_restart {
        msg.push_str("\nsmdjad restarted via ");
        msg.push_str(restart_method);
    }
    msg
}

async fn restart_smdjad_after_upgrade() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        return tokio::process::Command::new("systemctl")
            .args(["--user", "restart", "smdjad"])
            .status()
            .await
            .is_ok_and(|s| s.success())
            .then_some("systemctl");
    }

    #[cfg(target_os = "macos")]
    {
        let uid = tokio::process::Command::new("id")
            .arg("-u")
            .output()
            .await
            .ok()
            .and_then(|out| {
                out.status
                    .success()
                    .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            })?;
        return tokio::process::Command::new("launchctl")
            .args([
                "kickstart",
                "-k",
                &format!("gui/{uid}/nu.wigge.smedja.smdjad"),
            ])
            .status()
            .await
            .is_ok_and(|s| s.success())
            .then_some("launchctl");
    }

    #[allow(unreachable_code)]
    None
}

pub(crate) async fn run_openspec(bin: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("openspec exec error: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

/// Renders `openspec list --json` output into a human-readable string.
#[must_use]
pub(crate) fn format_openspec_list(json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => return format!("openspec list parse error: {e}"),
    };
    let changes = match v.get("changes").and_then(|c| c.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => return "no active changes".to_owned(),
    };
    let mut lines = vec!["active changes:".to_owned()];
    for c in changes {
        let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let status = c.get("status").and_then(|s| s.as_str()).unwrap_or("?");
        lines.push(format!("  {name:<30} {status}"));
    }
    lines.join("\n")
}

/// Renders `openspec status --json` output as `key: value` lines.
#[must_use]
pub(crate) fn format_openspec_status(json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => return format!("openspec status parse error: {e}"),
    };
    let Some(obj) = v.as_object() else {
        return "openspec status: unexpected response format".to_owned();
    };
    if obj.is_empty() {
        return "openspec status: no data".to_owned();
    }
    obj.iter()
        .map(|(k, v)| {
            let val = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
            format!("{k}: {val}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}
