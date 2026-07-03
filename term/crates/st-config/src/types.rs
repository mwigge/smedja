//! Public configuration types (font, window, key bindings, launch menu,
//! accessibility) and their defaults.

use serde::{Deserialize, Serialize};

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

/// Accessibility settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccessibilityConfig {
    /// When `true`, foreground colours are adjusted to meet WCAG AA (4.5:1)
    /// contrast ratio against their background before rendering.
    pub enforce_contrast: bool,
}

impl Default for AccessibilityConfig {
    fn default() -> Self {
        Self {
            enforce_contrast: true,
        }
    }
}

/// Returns the default key bindings.
pub(crate) fn default_key_bindings() -> Vec<KeyBinding> {
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
