use super::*;

pub(crate) async fn dispatch_term(action: TermCmd) -> Result<()> {
    match action {
        TermCmd::Install { bin_path, prefix } => {
            let prefix = prefix.unwrap_or_else(|| {
                std::env::var("HOME").map_or_else(
                    |_| PathBuf::from(".local/bin"),
                    |h| PathBuf::from(h).join(".local/bin"),
                )
            });
            let url = bin_path.unwrap_or_else(|| {
                let os = std::env::consts::OS;
                if os == "macos" {
                    "https://github.com/mwigge/smedja/releases/latest/download/smedja-darwin-x86_64.tar.gz".to_owned()
                } else {
                    "https://github.com/mwigge/smedja/releases/latest/download/smedja-linux-x86_64.tar.gz".to_owned()
                }
            });
            let prefix_clone = prefix.clone();
            let url_clone = url.clone();
            tokio::task::spawn_blocking(move || cmd_term_install(&url_clone, &prefix_clone))
                .await
                .context("install task panicked")??;
        }
        TermCmd::ConvertWezterm => {
            eprintln!("smj term convert-wezterm: not yet implemented");
            std::process::exit(1);
        }
    }
    Ok(())
}

pub(crate) fn cmd_term_install(url: &str, prefix: &std::path::Path) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::create_dir_all(prefix)
        .with_context(|| format!("cannot create prefix directory {}", prefix.display()))?;

    let dest = prefix.join("smedja");
    println!("Downloading smedja from {url} ...");

    let bytes = reqwest::blocking::get(url)
        .with_context(|| format!("download failed: {url}"))?
        .bytes()
        .with_context(|| "failed to read response bytes")?;

    let mut file = std::fs::File::create(&dest)
        .with_context(|| format!("cannot create {}", dest.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("cannot write {}", dest.display()))?;

    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("cannot chmod +x {}", dest.display()))?;

    println!("Installed smedja to {}", dest.display());

    // On Linux, write a .desktop file.
    if std::env::consts::OS == "linux" {
        if let Ok(home) = std::env::var("HOME") {
            let apps_dir = PathBuf::from(&home).join(".local/share/applications");
            let _ = std::fs::create_dir_all(&apps_dir);
            let desktop_path = apps_dir.join("smedja.desktop");
            let desktop = format!(
                "[Desktop Entry]\nVersion=1.0\nType=Application\nName=smedja\nExec={}\nIcon=utilities-terminal\nTerminal=false\nCategories=System;TerminalEmulator;\n",
                dest.display()
            );
            if let Ok(mut f) = std::fs::File::create(&desktop_path) {
                let _ = f.write_all(desktop.as_bytes());
                println!("Registered {}", desktop_path.display());
            }
        }
    }

    Ok(())
}
