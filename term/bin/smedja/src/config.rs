//! Launch-menu configuration loaded from `~/.config/smedja/config.toml`.

use tracing::debug;

// ── Launch menu entry ─────────────────────────────────────────────────────────

/// A single entry in the launch menu, loaded from `[[launch_menu]]` in
/// `~/.config/smedja/config.toml`.
#[derive(Debug, Clone)]
pub struct LaunchEntry {
    /// Display label shown in the overlay. Parsed from config; retained for the
    /// overlay UI even though it is not read on the current render path.
    #[allow(dead_code)]
    pub label: String,
    /// Command to execute in a new pane.
    pub command: String,
}

// ── Launch menu config loading ─────────────────────────────────────────────────

/// Loads `[[launch_menu]]` entries from the smedja config file.
///
/// The TOML format is:
/// ```toml
/// [[launch_menu]]
/// label   = "htop"
/// command = "htop"
///
/// [[launch_menu]]
/// label   = "neovim"
/// command = "nvim"
/// ```
///
/// Returns an empty `Vec` when the file is absent or the section is missing.
pub(crate) fn load_launch_entries() -> Vec<LaunchEntry> {
    #[derive(serde::Deserialize)]
    struct RawEntry {
        label: String,
        command: String,
    }

    #[derive(serde::Deserialize)]
    struct RawLaunchConfig {
        #[serde(default)]
        launch_menu: Vec<RawEntry>,
    }

    let path = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.config"))
        .join("smedja")
        .join("config.toml");

    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };

    let raw: RawLaunchConfig = match toml::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            debug!("launch_menu parse error: {}", e);
            return Vec::new();
        }
    };

    raw.launch_menu
        .into_iter()
        .map(|e| LaunchEntry {
            label: e.label,
            command: e.command,
        })
        .collect()
}
