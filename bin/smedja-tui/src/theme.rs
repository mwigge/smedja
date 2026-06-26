//! Forge palette — single source of truth for all TUI colours.
//!
//! Two distinct palettes live here:
//!
//! - **UI chrome** (`FORGE_*`) — amber/copper tones on near-black, matching
//!   the brand SVG mockups.  Used for borders, labels, status text, and all
//!   non-code UI surfaces.
//!
//! - **Code tokens** (`CODE_*`) — cool dark-theme tones (violet, green, cyan,
//!   ice-blue, orange).  Used exclusively in `code_widget` and `main_panel`
//!   syntax blocks so code content is visually distinct from chrome.
//!
//! All constants use `ratatui::style::Color::Rgb` so the rendered colours are
//! independent of the host terminal's 16-colour palette.
//!
//! ## Runtime overrides
//!
//! Call [`init_palette`] once at startup (before the first [`palette`] call)
//! to apply `[tui.colors]` overrides from the user's config file.  Widgets
//! read colours via [`palette()`] rather than the compile-time constants so
//! the override takes effect everywhere.

use std::sync::OnceLock;

use ratatui::style::Color;

// ---------------------------------------------------------------------------
// Compile-time defaults (used as Palette::default() fallbacks)
// ---------------------------------------------------------------------------

/// Main terminal background — `#0b0d0f` near-black from the SVG mockups.
pub const FORGE_BG: Color = Color::Rgb(11, 13, 15);
/// Inner panel fill — `#111316`, slightly lighter than the base.
pub const FORGE_PANEL: Color = Color::Rgb(17, 19, 22);
/// Header fill for title rows inside panels — `#211811` warm near-black.
pub const FORGE_HEADER: Color = Color::Rgb(33, 24, 17);

/// Primary border colour — `#a9652f` copper/rust (prominent panel outlines).
pub const FORGE_BORDER: Color = Color::Rgb(169, 101, 47);
/// Dim border — `#3b2a1f` for inner dividers and secondary outlines.
pub const FORGE_BORDER_DIM: Color = Color::Rgb(59, 42, 31);

/// Primary amber text — `#d99b55`.  Main body text, labels.
pub const FORGE_TEXT: Color = Color::Rgb(217, 155, 85);
/// Bright amber — `#f7c77e`.  Headings, highlights, active items.
pub const FORGE_TEXT_BRIGHT: Color = Color::Rgb(247, 199, 126);
/// Dim label — `#8f765b`.  Metadata, footers, secondary annotations.
pub const FORGE_TEXT_DIM: Color = Color::Rgb(143, 118, 91);
/// Accent amber — `#ffb24a`.  In-flight spinner highlights, selected rows.
pub const FORGE_ACCENT: Color = Color::Rgb(255, 178, 74);

/// Error state — `#d65f2e` forge red-orange (SVG traffic-light red).
pub const FORGE_ERROR: Color = Color::Rgb(214, 95, 46);
/// Success state — `#5d946b` forge green (SVG traffic-light green).
pub const FORGE_SUCCESS: Color = Color::Rgb(93, 148, 107);
/// Warning state — amber `#d99b55` (same as primary text — warm caution).
pub const FORGE_WARN: Color = Color::Rgb(217, 155, 85);

/// `local` tier — `#4eb9b2` warm teal (distinct from amber, still warm).
pub const FORGE_LOCAL: Color = Color::Rgb(78, 185, 178);
/// `fast` tier — `#f7c77e` bright gold (same as FORGE_TEXT_BRIGHT).
pub const FORGE_FAST: Color = Color::Rgb(247, 199, 126);
/// `deep` tier — `#a9652f` copper (same as FORGE_BORDER — the heavy hitter).
pub const FORGE_DEEP: Color = Color::Rgb(169, 101, 47);

/// Default code text — `#ccd0da` cool near-white.
pub const CODE_DEFAULT: Color = Color::Rgb(204, 208, 218);
/// Rust/language keywords — `#c792ea` soft violet.
pub const CODE_KEYWORD: Color = Color::Rgb(199, 146, 234);
/// String literals — `#c3e88d` soft green.
pub const CODE_STRING: Color = Color::Rgb(195, 232, 141);
/// Numeric literals — `#89ddff` light cyan.
pub const CODE_NUMBER: Color = Color::Rgb(137, 221, 255);
/// Comments — `#546e7a` dim teal-gray.
pub const CODE_COMMENT: Color = Color::Rgb(84, 110, 122);
/// Type identifiers and primitive types — `#82aaff` ice blue.
pub const CODE_TYPE: Color = Color::Rgb(130, 170, 255);
/// Macro invocations — `#f78c6c` warm orange.
pub const CODE_MACRO: Color = Color::Rgb(247, 140, 108);
/// Diff added lines — reuse FORGE_SUCCESS green.
pub const CODE_ADDED: Color = FORGE_SUCCESS;
/// Diff removed lines — reuse FORGE_ERROR red.
pub const CODE_REMOVED: Color = FORGE_ERROR;

// ---------------------------------------------------------------------------
// Runtime palette
// ---------------------------------------------------------------------------

/// Runtime-configurable colour palette.
///
/// All fields default to the forge compile-time constants above.  Call
/// [`init_palette`] once at startup (before the first [`palette`] call) to
/// apply any `[tui.colors]` overrides from the user's config file.
#[derive(Debug, Clone)]
pub struct Palette {
    // UI chrome
    pub bg: Color,
    pub panel: Color,
    pub header: Color,
    pub border: Color,
    pub border_dim: Color,
    pub text: Color,
    pub text_bright: Color,
    pub text_dim: Color,
    pub accent: Color,
    pub error: Color,
    pub success: Color,
    pub warn: Color,
    pub local: Color,
    pub fast: Color,
    pub deep: Color,
    // Code tokens
    pub code_default: Color,
    pub code_keyword: Color,
    pub code_string: Color,
    pub code_number: Color,
    pub code_comment: Color,
    pub code_type: Color,
    pub code_macro: Color,
    pub code_added: Color,
    pub code_removed: Color,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            bg: FORGE_BG,
            panel: FORGE_PANEL,
            header: FORGE_HEADER,
            border: FORGE_BORDER,
            border_dim: FORGE_BORDER_DIM,
            text: FORGE_TEXT,
            text_bright: FORGE_TEXT_BRIGHT,
            text_dim: FORGE_TEXT_DIM,
            accent: FORGE_ACCENT,
            error: FORGE_ERROR,
            success: FORGE_SUCCESS,
            warn: FORGE_WARN,
            local: FORGE_LOCAL,
            fast: FORGE_FAST,
            deep: FORGE_DEEP,
            code_default: CODE_DEFAULT,
            code_keyword: CODE_KEYWORD,
            code_string: CODE_STRING,
            code_number: CODE_NUMBER,
            code_comment: CODE_COMMENT,
            code_type: CODE_TYPE,
            code_macro: CODE_MACRO,
            code_added: CODE_ADDED,
            code_removed: CODE_REMOVED,
        }
    }
}

// ---------------------------------------------------------------------------
// TOML config schema — [tui.colors]
// ---------------------------------------------------------------------------

/// Per-slot colour overrides from `[tui.colors]` in the user config file.
///
/// Each field is an optional hex string (`"#rrggbb"`).  Missing keys fall
/// back to the forge defaults.  Unknown fields are silently ignored by serde.
#[derive(Debug, Default, serde::Deserialize)]
pub struct TuiColorConfig {
    // UI chrome
    pub bg: Option<String>,
    pub panel: Option<String>,
    pub header: Option<String>,
    pub border: Option<String>,
    pub border_dim: Option<String>,
    pub text: Option<String>,
    pub text_bright: Option<String>,
    pub text_dim: Option<String>,
    pub accent: Option<String>,
    pub error: Option<String>,
    pub success: Option<String>,
    pub warn: Option<String>,
    pub local: Option<String>,
    pub fast: Option<String>,
    pub deep: Option<String>,
    // Code tokens
    pub code_default: Option<String>,
    pub code_keyword: Option<String>,
    pub code_string: Option<String>,
    pub code_number: Option<String>,
    pub code_comment: Option<String>,
    pub code_type: Option<String>,
    pub code_macro: Option<String>,
    pub code_added: Option<String>,
    pub code_removed: Option<String>,
}

// ---------------------------------------------------------------------------
// Global palette singleton
// ---------------------------------------------------------------------------

static PALETTE: OnceLock<Palette> = OnceLock::new();

/// Returns a reference to the active runtime palette.
///
/// If [`init_palette`] has not been called, returns the default forge palette.
/// The returned reference is `'static` — safe to call from any widget render
/// function without holding a lock.
pub fn palette() -> &'static Palette {
    PALETTE.get_or_init(Palette::default)
}

/// Initialises the global palette from optional `[tui.colors]` config overrides.
///
/// Must be called before the first [`palette`] call (i.e. before the TUI
/// render loop starts).  A second call is silently ignored because
/// `OnceLock::set` is a no-op when already initialised.
///
/// Hex strings that cannot be parsed are ignored; the slot keeps its default.
pub fn init_palette(cfg: Option<&TuiColorConfig>) {
    let mut p = Palette::default();
    if let Some(c) = cfg {
        macro_rules! apply {
            ($field:ident) => {
                if let Some(ref s) = c.$field {
                    if let Some(color) = parse_hex(s) {
                        p.$field = color;
                    }
                }
            };
        }
        apply!(bg);
        apply!(panel);
        apply!(header);
        apply!(border);
        apply!(border_dim);
        apply!(text);
        apply!(text_bright);
        apply!(text_dim);
        apply!(accent);
        apply!(error);
        apply!(success);
        apply!(warn);
        apply!(local);
        apply!(fast);
        apply!(deep);
        apply!(code_default);
        apply!(code_keyword);
        apply!(code_string);
        apply!(code_number);
        apply!(code_comment);
        apply!(code_type);
        apply!(code_macro);
        apply!(code_added);
        apply!(code_removed);
    }
    let _ = PALETTE.set(p);
}

/// Parses a `#rrggbb` hex string into a `Color::Rgb`.
fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

// ---------------------------------------------------------------------------
// Legacy agent_theme map (used by tests; kept for API stability)
// ---------------------------------------------------------------------------

use std::collections::HashMap;

/// Returns the canonical agent theme colour map using the runtime palette.
///
/// Keys: `"local"`, `"fast"`, `"deep"`, `"error"`, `"success"`, `"warn"`,
/// `"border"`, `"highlight"`.
#[must_use]
pub fn agent_theme() -> HashMap<&'static str, Color> {
    let p = palette();
    let mut m = HashMap::new();
    m.insert("local", p.local);
    m.insert("fast", p.fast);
    m.insert("deep", p.deep);
    m.insert("error", p.error);
    m.insert("success", p.success);
    m.insert("warn", p.warn);
    m.insert("border", p.border);
    m.insert("highlight", p.text_bright);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_eight_keys_present() {
        let theme = agent_theme();
        for key in &[
            "local", "fast", "deep", "error", "success", "warn", "border", "highlight",
        ] {
            assert!(theme.contains_key(key), "missing key: {key}");
        }
        assert_eq!(theme.len(), 8);
    }

    #[test]
    fn forge_colours_are_rgb() {
        assert!(matches!(FORGE_BG, Color::Rgb(_, _, _)));
        assert!(matches!(FORGE_BORDER, Color::Rgb(_, _, _)));
        assert!(matches!(CODE_KEYWORD, Color::Rgb(_, _, _)));
    }

    #[test]
    fn error_and_success_are_deterministic() {
        assert_eq!(agent_theme().get("error"), Some(&FORGE_ERROR));
        assert_eq!(agent_theme().get("success"), Some(&FORGE_SUCCESS));
    }

    #[test]
    fn palette_defaults_match_constants() {
        let p = Palette::default();
        assert_eq!(p.bg, FORGE_BG);
        assert_eq!(p.border, FORGE_BORDER);
        assert_eq!(p.error, FORGE_ERROR);
        assert_eq!(p.success, FORGE_SUCCESS);
        assert_eq!(p.code_keyword, CODE_KEYWORD);
    }

    #[test]
    fn parse_hex_valid() {
        assert_eq!(parse_hex("#0b0d0f"), Some(Color::Rgb(11, 13, 15)));
        assert_eq!(parse_hex("d99b55"), Some(Color::Rgb(217, 155, 85)));
        assert_eq!(parse_hex("#ffffff"), Some(Color::Rgb(255, 255, 255)));
    }

    #[test]
    fn parse_hex_invalid_returns_none() {
        assert!(parse_hex("").is_none());
        assert!(parse_hex("#gg0000").is_none());
        assert!(parse_hex("#0011").is_none());
    }

    #[test]
    fn tui_color_config_applies_overrides() {
        let cfg = TuiColorConfig {
            bg: Some("#ff0000".to_owned()),
            border: Some("00ff00".to_owned()),
            ..TuiColorConfig::default()
        };
        let mut p = Palette::default();
        // Apply manually (mirrors init_palette logic without touching OnceLock).
        if let Some(ref s) = cfg.bg { if let Some(c) = parse_hex(s) { p.bg = c; } }
        if let Some(ref s) = cfg.border { if let Some(c) = parse_hex(s) { p.border = c; } }
        assert_eq!(p.bg, Color::Rgb(255, 0, 0));
        assert_eq!(p.border, Color::Rgb(0, 255, 0));
        assert_eq!(p.text, FORGE_TEXT, "unset slots keep the default");
    }
}
