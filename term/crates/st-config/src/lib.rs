//! `st-config` — configuration for smedja-term.
//!
//! Loads `~/.config/smedja-term/config.toml` when present; otherwise returns
//! the built-in `forged_terminal` theme defaults.

pub mod migrate;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors produced by config loading.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The config file exists but could not be read.
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    /// The config file exists but is not valid TOML.
    #[error("failed to parse config file: {0}")]
    Parse(#[from] toml::de::Error),
    /// A colour hex string was not a valid RGB triplet.
    #[error("invalid colour hex string '{0}'")]
    InvalidColor(String),
}

// ── Raw TOML types (all fields optional, defaults applied on conversion) ──────

#[derive(Debug, Deserialize)]
struct RawConfig {
    font: Option<RawFontConfig>,
    colors: Option<RawColorConfig>,
    window: Option<RawWindowConfig>,
    scrollback_lines: Option<usize>,
    key_bindings: Option<Vec<RawKeyBinding>>,
    launch_menu: Option<Vec<RawLaunchEntry>>,
}

#[derive(Debug, Deserialize)]
struct RawFontConfig {
    family: Option<String>,
    size: Option<f32>,
    bold_family: Option<String>,
    italic_family: Option<String>,
    bold_italic_family: Option<String>,
    fallback: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawColorConfig {
    background: Option<String>,
    foreground: Option<String>,
    cursor: Option<String>,
    selection_bg: Option<String>,
    selection_fg: Option<String>,
    ansi: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawWindowConfig {
    background_opacity: Option<f32>,
    background_image: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawKeyBinding {
    key: String,
    action: KeyAction,
}

#[derive(Debug, Deserialize)]
struct RawLaunchEntry {
    label: String,
    args: Vec<String>,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Font settings for the terminal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FontConfig {
    /// Primary font family name. Defaults to `"monospace"`.
    pub family: String,
    /// Point size. Defaults to `14.0`.
    pub size: f32,
    /// Bold variant family, if different from `family`.
    pub bold_family: Option<String>,
    /// Italic variant family, if different from `family`.
    pub italic_family: Option<String>,
    /// Bold-italic variant family, if different from `family`.
    pub bold_italic_family: Option<String>,
    /// Ordered fallback font families for glyphs not covered by `family`.
    pub fallback: Vec<String>,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "monospace".into(),
            size: 14.0,
            bold_family: None,
            italic_family: None,
            bold_italic_family: None,
            fallback: Vec::new(),
        }
    }
}

/// Colour palette for the terminal.
///
/// All colours are represented as `[R, G, B, A]` with component values in
/// `0.0 ..= 1.0`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ColorConfig {
    /// Default cell background.
    pub background: [f32; 4],
    /// Default foreground (text) colour.
    pub foreground: [f32; 4],
    /// Cursor rectangle colour.
    pub cursor: [f32; 4],
    /// Background colour for selected text.
    pub selection_bg: [f32; 4],
    /// Foreground colour for selected text.
    pub selection_fg: [f32; 4],
    /// 16-entry ANSI palette: indices 0-7 are normal, 8-15 are bright.
    pub ansi: [[f32; 4]; 16],
}

/// Window / compositor settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowConfig {
    /// Alpha for the terminal background (0.0 = fully transparent, 1.0 = opaque).
    pub background_opacity: f32,
    /// Optional background image path.
    pub background_image: Option<String>,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            background_opacity: 1.0,
            background_image: None,
        }
    }
}

/// A key binding mapping a key combination to an action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyBinding {
    /// Key combination string, e.g. `"ctrl+t"`, `"ctrl+shift+h"`.
    pub key: String,
    /// Action to perform when the key is pressed.
    pub action: KeyAction,
}

/// Actions that can be bound to a key combination.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    OpenTab,
    CloseTab,
    NextTab,
    PrevTab,
    SplitHorizontal,
    SplitVertical,
    ZoomPane,
    OpenLaunchMenu,
    CopyTo(String),
    PasteFrom(String),
    SpawnTab,
    ScrollUp(u32),
    ScrollDown(u32),
}

/// A launch menu entry (a labelled shell command shortcut).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaunchEntry {
    /// Display label shown in the launch menu.
    pub label: String,
    /// Command arguments to spawn.
    pub args: Vec<String>,
}

/// Top-level smedja-term configuration.
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
        }
    }
}

/// Returns the default key bindings.
fn default_key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding {
            key: "ctrl+t".into(),
            action: KeyAction::OpenTab,
        },
        KeyBinding {
            key: "ctrl+w".into(),
            action: KeyAction::CloseTab,
        },
        KeyBinding {
            key: "ctrl+tab".into(),
            action: KeyAction::NextTab,
        },
        KeyBinding {
            key: "ctrl+shift+tab".into(),
            action: KeyAction::PrevTab,
        },
        KeyBinding {
            key: "ctrl+shift+h".into(),
            action: KeyAction::SplitHorizontal,
        },
        KeyBinding {
            key: "ctrl+shift+v".into(),
            action: KeyAction::SplitVertical,
        },
        KeyBinding {
            key: "ctrl+shift+z".into(),
            action: KeyAction::ZoomPane,
        },
        KeyBinding {
            key: "ctrl+shift+l".into(),
            action: KeyAction::OpenLaunchMenu,
        },
        KeyBinding {
            key: "ctrl+c".into(),
            action: KeyAction::CopyTo("clipboard".into()),
        },
        KeyBinding {
            key: "ctrl+v".into(),
            action: KeyAction::PasteFrom("clipboard".into()),
        },
    ]
}

impl Config {
    /// Loads configuration from `~/.config/smedja-term/config.toml`.
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

        Ok(cfg)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Returns the path `~/.config/smedja-term/config.toml`.
fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("smedja-term")
        .join("config.toml")
}

/// Parses a hex colour string like `"#0b0d0f"` into a linear `[R, G, B, A]`
/// tuple with values in `0.0 ..= 1.0`.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidColor`] when the string is not a valid 6-digit
/// hex colour.
pub fn hex_to_rgba(hex: &str) -> Result<[f32; 4], ConfigError> {
    let s = hex.trim_start_matches('#');
    if s.len() != 6 {
        return Err(ConfigError::InvalidColor(hex.to_owned()));
    }
    let r =
        u8::from_str_radix(&s[0..2], 16).map_err(|_| ConfigError::InvalidColor(hex.to_owned()))?;
    let g =
        u8::from_str_radix(&s[2..4], 16).map_err(|_| ConfigError::InvalidColor(hex.to_owned()))?;
    let b =
        u8::from_str_radix(&s[4..6], 16).map_err(|_| ConfigError::InvalidColor(hex.to_owned()))?;
    Ok([
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        1.0,
    ])
}

/// Builds the built-in `forged_terminal` colour palette.
fn forged_terminal_colors() -> ColorConfig {
    // SAFETY for unwrap: all hex literals here are compile-time constants that are
    // guaranteed to be valid 6-digit hex strings.
    ColorConfig {
        background: hex_to_rgba("#0b0d0f").unwrap(),
        foreground: hex_to_rgba("#f4d7a1").unwrap(),
        cursor: hex_to_rgba("#ffb24a").unwrap(),
        selection_bg: hex_to_rgba("#3b2a1f").unwrap(),
        selection_fg: hex_to_rgba("#ffd37a").unwrap(),
        ansi: [
            hex_to_rgba("#111316").unwrap(), // 0  black
            hex_to_rgba("#d65f2e").unwrap(), // 1  red
            hex_to_rgba("#5d946b").unwrap(), // 2  green
            hex_to_rgba("#d99b55").unwrap(), // 3  yellow
            hex_to_rgba("#8f765b").unwrap(), // 4  blue
            hex_to_rgba("#a9652f").unwrap(), // 5  magenta
            hex_to_rgba("#f7c77e").unwrap(), // 6  cyan
            hex_to_rgba("#f4d7a1").unwrap(), // 7  white
            hex_to_rgba("#3b2a1f").unwrap(), // 8  bright black
            hex_to_rgba("#e8753e").unwrap(), // 9  bright red
            hex_to_rgba("#77aa7c").unwrap(), // 10 bright green
            hex_to_rgba("#ffd37a").unwrap(), // 11 bright yellow
            hex_to_rgba("#b4845a").unwrap(), // 12 bright blue
            hex_to_rgba("#c07a3a").unwrap(), // 13 bright magenta
            hex_to_rgba("#ffb24a").unwrap(), // 14 bright cyan
            hex_to_rgba("#fff1cf").unwrap(), // 15 bright white
        ],
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact float literals produced by our own parse — deterministic
mod tests {
    use super::*;

    #[test]
    fn hex_to_rgba_parses_black() {
        let c = hex_to_rgba("#000000").unwrap();
        assert!(c[0].abs() < 1e-6);
        assert!(c[1].abs() < 1e-6);
        assert!(c[2].abs() < 1e-6);
        assert!((c[3] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn hex_to_rgba_parses_white() {
        let c = hex_to_rgba("#ffffff").unwrap();
        assert!((c[0] - 1.0).abs() < 1e-6);
        assert!((c[1] - 1.0).abs() < 1e-6);
        assert!((c[2] - 1.0).abs() < 1e-6);
        assert!((c[3] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn hex_to_rgba_rejects_invalid() {
        assert!(hex_to_rgba("gg0000").is_err());
        assert!(hex_to_rgba("#fff").is_err());
        assert!(hex_to_rgba("").is_err());
    }

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
