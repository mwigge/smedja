//! Raw TOML deserialization types.
//!
//! All fields are optional; defaults are applied when converting into the
//! public [`Config`](crate::Config) via [`Config::from_toml_str`](crate::Config::from_toml_str).

use serde::Deserialize;

use crate::KeyAction;

#[derive(Debug, Deserialize)]
pub(crate) struct RawConfig {
    pub(crate) font: Option<RawFontConfig>,
    pub(crate) colors: Option<RawColorConfig>,
    pub(crate) window: Option<RawWindowConfig>,
    pub(crate) scrollback_lines: Option<usize>,
    pub(crate) key_bindings: Option<Vec<RawKeyBinding>>,
    pub(crate) launch_menu: Option<Vec<RawLaunchEntry>>,
    pub(crate) accessibility: Option<RawAccessibilityConfig>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawAccessibilityConfig {
    pub(crate) enforce_contrast: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawFontConfig {
    pub(crate) family: Option<String>,
    pub(crate) size: Option<f32>,
    pub(crate) bold_family: Option<String>,
    pub(crate) italic_family: Option<String>,
    pub(crate) bold_italic_family: Option<String>,
    pub(crate) fallback: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawColorConfig {
    pub(crate) background: Option<String>,
    pub(crate) foreground: Option<String>,
    pub(crate) cursor: Option<String>,
    pub(crate) selection_bg: Option<String>,
    pub(crate) selection_fg: Option<String>,
    pub(crate) ansi: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawWindowConfig {
    pub(crate) background_opacity: Option<f32>,
    pub(crate) background_image: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawKeyBinding {
    pub(crate) key: String,
    pub(crate) action: KeyAction,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawLaunchEntry {
    pub(crate) label: String,
    pub(crate) args: Vec<String>,
}
