//! Top-level [`Config`] type: loading, TOML parsing, and defaults.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::colors::{forged_terminal_colors, hex_to_rgba, ColorConfig};
use crate::raw::RawConfig;
use crate::types::{
    default_key_bindings, AccessibilityConfig, FontConfig, KeyBinding, LaunchEntry, WindowConfig,
};
use crate::ConfigError;

/// Top-level smedja configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// Font settings.
    pub font: FontConfig,
    /// Colour palette.
    pub colors: ColorConfig,
    /// Window / compositor settings.
    pub window: WindowConfig,
    /// Maximum number of lines kept in the scrollback buffer.
    pub scrollback_lines: usize,
    /// Key bindings active in this session.
    pub key_bindings: Vec<KeyBinding>,
    /// Launch menu entries shown in the quick-launch overlay.
    pub launch_menu: Vec<LaunchEntry>,
    /// Accessibility settings.
    pub accessibility: AccessibilityConfig,
}

impl Default for Config {
    /// Returns the built-in `forged_terminal` theme.
    fn default() -> Self {
        Self {
            font: FontConfig::default(),
            colors: forged_terminal_colors(),
            window: WindowConfig::default(),
            scrollback_lines: 10_000,
            key_bindings: default_key_bindings(),
            launch_menu: Vec::new(),
            accessibility: AccessibilityConfig::default(),
        }
    }
}

impl Config {
    /// Loads configuration from `~/.config/smedja/config.toml`.
    ///
    /// Falls back to [`Config::default`] (the `forged_terminal` theme) when the
    /// file is absent.  Returns an error only when the file exists but cannot be
    /// parsed.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the config file exists but is unreadable or
    /// contains invalid TOML / invalid colour values.
    pub fn load() -> Result<Self, ConfigError> {
        let path = config_path();
        if !path.exists() {
            tracing::debug!("config file not found at {:?}, using defaults", path);
            return Ok(Self::default());
        }
        tracing::debug!("loading config from {:?}", path);
        let text = std::fs::read_to_string(&path)?;
        Self::from_toml_str(&text)
    }

    /// Parses a TOML string into a [`Config`], merging with defaults.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] on parse or colour conversion failure.
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = toml::from_str(text)?;
        let mut cfg = Self::default();

        if let Some(f) = raw.font {
            if let Some(v) = f.family {
                cfg.font.family = v;
            }
            if let Some(v) = f.size {
                cfg.font.size = v;
            }
            cfg.font.bold_family = f.bold_family.or(cfg.font.bold_family);
            cfg.font.italic_family = f.italic_family.or(cfg.font.italic_family);
            cfg.font.bold_italic_family = f.bold_italic_family.or(cfg.font.bold_italic_family);
            if let Some(v) = f.fallback {
                cfg.font.fallback = v;
            }
        }

        if let Some(c) = raw.colors {
            if let Some(v) = c.background {
                cfg.colors.background = hex_to_rgba(&v)?;
            }
            if let Some(v) = c.foreground {
                cfg.colors.foreground = hex_to_rgba(&v)?;
            }
            if let Some(v) = c.cursor {
                cfg.colors.cursor = hex_to_rgba(&v)?;
            }
            if let Some(v) = c.selection_bg {
                cfg.colors.selection_bg = hex_to_rgba(&v)?;
            }
            if let Some(v) = c.selection_fg {
                cfg.colors.selection_fg = hex_to_rgba(&v)?;
            }
            if let Some(ansi) = c.ansi {
                for (i, hex) in ansi.iter().enumerate().take(16) {
                    cfg.colors.ansi[i] = hex_to_rgba(hex)?;
                }
            }
        }

        if let Some(w) = raw.window {
            if let Some(v) = w.background_opacity {
                cfg.window.background_opacity = v.clamp(0.0, 1.0);
            }
            cfg.window.background_image = w.background_image.or(cfg.window.background_image);
        }

        if let Some(v) = raw.scrollback_lines {
            cfg.scrollback_lines = v;
        }

        if let Some(bindings) = raw.key_bindings {
            cfg.key_bindings = bindings
                .into_iter()
                .map(|rb| KeyBinding {
                    key: rb.key,
                    action: rb.action,
                })
                .collect();
        }

        if let Some(entries) = raw.launch_menu {
            cfg.launch_menu = entries
                .into_iter()
                .map(|re| LaunchEntry {
                    label: re.label,
                    args: re.args,
                })
                .collect();
        }

        if let Some(a) = raw.accessibility {
            if let Some(v) = a.enforce_contrast {
                cfg.accessibility.enforce_contrast = v;
            }
        }

        Ok(cfg)
    }
}

/// Returns the path `~/.config/smedja/config.toml`.
fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("smedja")
        .join("config.toml")
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact float literals produced by our own parse — deterministic
mod tests {
    use super::*;
    use crate::hex_to_rgba;
    use crate::KeyAction;

    #[test]
    fn default_config_has_forged_terminal_background() {
        let cfg = Config::default();
        // forged_terminal background = #0b0d0f
        let expected = hex_to_rgba("#0b0d0f").unwrap();
        for (a, b) in cfg.colors.background.iter().zip(expected.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn default_config_scrollback_is_10000() {
        assert_eq!(Config::default().scrollback_lines, 10_000);
    }

    #[test]
    fn from_toml_str_overrides_font_size() {
        let toml = "[font]\nsize = 18.5\n";
        let cfg = Config::from_toml_str(toml).unwrap();
        assert!((cfg.font.size - 18.5).abs() < 1e-6);
        // other defaults preserved
        assert_eq!(cfg.font.family, "monospace");
    }

    #[test]
    fn from_toml_str_overrides_background() {
        let toml = "[colors]\nbackground = \"#ff0000\"\n";
        let cfg = Config::from_toml_str(toml).unwrap();
        assert!((cfg.colors.background[0] - 1.0).abs() < 1e-6);
        assert!(cfg.colors.background[1].abs() < 1e-6);
    }

    #[test]
    fn from_toml_str_rejects_invalid_hex() {
        let toml = "[colors]\nbackground = \"not-a-color\"\n";
        assert!(Config::from_toml_str(toml).is_err());
    }

    #[test]
    fn from_toml_str_window_opacity_clamped() {
        let toml = "[window]\nbackground_opacity = 2.5\n";
        let cfg = Config::from_toml_str(toml).unwrap();
        assert!((cfg.window.background_opacity - 1.0).abs() < 1e-6);
    }

    #[test]
    fn from_toml_str_ansi_16_entries() {
        // Providing all 16 ANSI colours
        let toml = "[colors]\nansi = [\n  \"#000000\",\"#111111\",\"#222222\",\"#333333\",\n  \"#444444\",\"#555555\",\"#666666\",\"#777777\",\n  \"#888888\",\"#999999\",\"#aaaaaa\",\"#bbbbbb\",\n  \"#cccccc\",\"#dddddd\",\"#eeeeee\",\"#ffffff\",\n]\n";
        let cfg = Config::from_toml_str(toml).unwrap();
        let first = cfg.colors.ansi[0];
        assert!(first[0].abs() < 1e-6, "ansi[0][0] should be 0");
        assert!(first[1].abs() < 1e-6, "ansi[0][1] should be 0");
        assert!(first[2].abs() < 1e-6, "ansi[0][2] should be 0");
        assert!((first[3] - 1.0).abs() < 1e-6, "ansi[0][3] should be 1");
        let last = cfg.colors.ansi[15];
        assert!((last[0] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn config_load_falls_back_to_default_when_no_file() {
        // Provide a non-existent path by temporarily pointing dirs at /tmp via
        // an env-var free path — we just call load() and it must not error even
        // when no config file is present in the test environment.
        // This test does not error; if a real config file exists it just returns
        // its contents; either path is acceptable for this contract.
        let result = Config::load();
        assert!(result.is_ok());
    }

    #[test]
    fn default_config_has_open_tab_binding() {
        let cfg = Config::default();
        assert!(
            cfg.key_bindings
                .iter()
                .any(|b| b.key == "ctrl+t" && b.action == KeyAction::OpenTab),
            "expected ctrl+t → OpenTab in default bindings"
        );
    }

    #[test]
    fn from_toml_str_parses_key_binding_section() {
        let toml = r#"
[[key_bindings]]
key = "ctrl+shift+x"
action = "spawn_tab"
"#;
        let cfg = Config::from_toml_str(toml).unwrap();
        // Default bindings are replaced when key_bindings is specified.
        assert!(cfg
            .key_bindings
            .iter()
            .any(|b| b.key == "ctrl+shift+x" && b.action == KeyAction::SpawnTab));
    }

    #[test]
    fn launch_entry_roundtrips_toml() {
        let toml = r#"
[[launch_menu]]
label = "htop"
args = ["htop"]
"#;
        let cfg = Config::from_toml_str(toml).unwrap();
        assert_eq!(cfg.launch_menu.len(), 1);
        assert_eq!(cfg.launch_menu[0].label, "htop");
        assert_eq!(cfg.launch_menu[0].args, vec!["htop"]);
    }
}
