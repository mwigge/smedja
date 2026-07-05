//! Forge palette — single source of truth for all TUI colours.
//!
//! Two distinct palettes live here:
//!
//! - **UI chrome** (`FORGE_*`) — amber/copper tones on near-black, matching
//!   the brand SVG mockups.  Used for borders, labels, status text, and all
//!   non-code UI surfaces.
//!
//! - **Code tokens** (`CODE_*`) — cool dark-theme tones (violet, green, cyan,
//!   ice-blue, orange).  Used exclusively in `main_panel` syntax blocks so code
//!   content is visually distinct from chrome.
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
/// Inner panel fill — `#181b1f`, lifted off the base so panels read as raised.
pub const FORGE_PANEL: Color = Color::Rgb(24, 27, 31);
/// Header fill for title rows inside panels — `#2b1f15` warm near-black.
pub const FORGE_HEADER: Color = Color::Rgb(43, 31, 21);

/// Primary border colour — `#c98042` copper/rust (prominent panel outlines).
pub const FORGE_BORDER: Color = Color::Rgb(201, 128, 66);
/// Dim border — `#68503c` for inner dividers — bright enough to actually see.
pub const FORGE_BORDER_DIM: Color = Color::Rgb(104, 80, 60);

/// Primary amber text — `#ebb67c`.  Main body text, labels (lifted for contrast).
pub const FORGE_TEXT: Color = Color::Rgb(235, 182, 124);
/// Bright amber — `#ffda9c`.  Headings, highlights, active items.
pub const FORGE_TEXT_BRIGHT: Color = Color::Rgb(255, 218, 156);
/// Dim label — `#b6a28a`.  Metadata, footers — dimmer than body but legible.
pub const FORGE_TEXT_DIM: Color = Color::Rgb(182, 162, 138);
/// Accent amber — `#ffb24a`.  In-flight spinner highlights, selected rows.
pub const FORGE_ACCENT: Color = Color::Rgb(255, 178, 74);
/// Molten lava-orange — `#e8521a`.  The signature forge accent the brand leads
/// with: the primary highlight for the prompt indicator, the in-flight spinner,
/// and the active/selected row.  Hotter and more saturated than `FORGE_ACCENT`.
pub const FORGE_MOLTEN: Color = Color::Rgb(232, 82, 26);

/// Error state — `#f07848` forge red-orange (SVG traffic-light red).
pub const FORGE_ERROR: Color = Color::Rgb(240, 120, 72);
/// Success state — `#7aca8e` forge green (SVG traffic-light green, brightened).
pub const FORGE_SUCCESS: Color = Color::Rgb(122, 202, 142);
/// Warning state — yellow `#f3c55c`, distinct from primary amber text.
pub const FORGE_WARN: Color = Color::Rgb(243, 197, 92);

/// `local` tier — `#4eb9b2` warm teal (distinct from amber, still warm).
pub const FORGE_LOCAL: Color = Color::Rgb(78, 185, 178);
/// `fast` tier — `#f7c77e` bright gold (same as `FORGE_TEXT_BRIGHT`).
pub const FORGE_FAST: Color = Color::Rgb(247, 199, 126);
/// `deep` tier — `#a9652f` copper (same as `FORGE_BORDER` — the heavy hitter).
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
/// Diff added lines — reuse `FORGE_SUCCESS` green.
pub const CODE_ADDED: Color = FORGE_SUCCESS;
/// Diff removed lines — reuse `FORGE_ERROR` red.
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
    pub molten: Color,
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
            molten: FORGE_MOLTEN,
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
    pub molten: Option<String>,
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
        apply!(molten);
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

/// Normalises a runner/client name to its lowercase key, dropping a `-cli`
/// suffix (`claude-cli` → `claude`).
fn runner_key(runner: &str) -> String {
    let lower = runner.trim().to_ascii_lowercase();
    lower
        .strip_suffix("-cli")
        .map_or(lower.clone(), str::to_owned)
}

/// Brand accent colour for a runner/client. Distinct, high-contrast hues on the
/// dark forge background; unknown runners fall back to the configured accent.
#[must_use]
pub fn runner_color(runner: &str) -> Color {
    match runner_key(runner).as_str() {
        "claude" | "anthropic" => Color::Rgb(0xC9, 0x7A, 0x40), // copper
        "codex" | "openai" => Color::Rgb(0x10, 0xA3, 0x7F),     // openai green
        "copilot" | "github" => Color::Rgb(0x3B, 0x8E, 0xEA),   // azure
        "minimax" => Color::Rgb(0xA0, 0x6C, 0xD5),              // violet
        "gemini" | "google" => Color::Rgb(0x53, 0x8B, 0xF0),    // blue
        "berget" => Color::Rgb(0xE0, 0x8C, 0x52),               // ember
        "local" => palette().local,                             // teal
        _ => palette().accent,
    }
}

/// Short uppercase display label for a runner (`claude-cli` → `CLAUDE`).
#[must_use]
pub fn runner_label(runner: &str) -> String {
    runner_key(runner).to_ascii_uppercase()
}

/// Curated, colourblind-aware accent palette for per-agent identity. Picked for
/// mutual distinctness on the dark forge background. Used ONLY as accents (left
/// borders, badge pips) — never to recolour body text, which is what made the
/// milliways per-agent colouring hard to read.
const AGENT_PALETTE: [Color; 6] = [
    Color::Rgb(0x6C, 0xB6, 0xFF), // sky
    Color::Rgb(0x9D, 0xD6, 0x7D), // green
    Color::Rgb(0xF2, 0xB6, 0x6D), // amber
    Color::Rgb(0xD6, 0x8C, 0xE0), // orchid
    Color::Rgb(0x6F, 0xD6, 0xC4), // teal
    Color::Rgb(0xF2, 0x8C, 0x8C), // salmon
];

/// Deterministic accent colour for an agent/role name — stable across runs so
/// an agent keeps the same colour everywhere it appears (FNV-1a over the
/// lowercased name into [`AGENT_PALETTE`]).
#[must_use]
pub fn agent_color(name: &str) -> Color {
    let mut hash: u32 = 2_166_136_261;
    for b in name.trim().to_ascii_lowercase().bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(16_777_619);
    }
    AGENT_PALETTE[(hash as usize) % AGENT_PALETTE.len()]
}

/// Returns a readable foreground (near-black or bright forge text) for text
/// drawn on the coloured `bg`, chosen by Rec. 601 luma. The auto-contrast trick
/// borrowed from semos-labs/glyph so badge text stays legible on any accent.
#[must_use]
pub fn contrast_fg(bg: Color) -> Color {
    if let Color::Rgb(r, g, b) = bg {
        let luma = 0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b);
        if luma > 140.0 {
            FORGE_BG
        } else {
            FORGE_TEXT_BRIGHT
        }
    } else {
        FORGE_TEXT_BRIGHT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_eight_keys_present() {
        let theme = agent_theme();
        for key in &[
            "local",
            "fast",
            "deep",
            "error",
            "success",
            "warn",
            "border",
            "highlight",
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
    fn runner_label_strips_cli_suffix_and_uppercases() {
        assert_eq!(runner_label("claude-cli"), "CLAUDE");
        assert_eq!(runner_label("codex-cli"), "CODEX");
        assert_eq!(runner_label("copilot"), "COPILOT");
        assert_eq!(runner_label("  MiniMax  "), "MINIMAX");
    }

    #[test]
    fn agent_color_is_deterministic_and_in_palette() {
        // Same name → same colour every time (stable identity).
        assert_eq!(agent_color("reviewer"), agent_color("reviewer"));
        assert_eq!(
            agent_color("Reviewer"),
            agent_color("reviewer"),
            "case-insensitive"
        );
        // Result is always one of the curated palette entries.
        assert!(AGENT_PALETTE.contains(&agent_color("planner")));
        assert!(AGENT_PALETTE.contains(&agent_color("implementer")));
    }

    #[test]
    fn contrast_fg_picks_dark_on_light_and_bright_on_dark() {
        assert_eq!(
            contrast_fg(Color::Rgb(240, 240, 240)),
            FORGE_BG,
            "dark text on light bg"
        );
        assert_eq!(
            contrast_fg(Color::Rgb(20, 20, 20)),
            FORGE_TEXT_BRIGHT,
            "bright text on dark bg"
        );
    }

    #[test]
    fn runner_color_is_distinct_per_brand() {
        let claude = runner_color("claude-cli");
        let codex = runner_color("codex");
        let copilot = runner_color("copilot");
        assert!(matches!(claude, Color::Rgb(_, _, _)));
        assert_ne!(claude, codex);
        assert_ne!(codex, copilot);
        assert_ne!(claude, copilot);
        // -cli suffix is irrelevant to the colour.
        assert_eq!(runner_color("claude"), runner_color("claude-cli"));
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
        assert_eq!(p.molten, FORGE_MOLTEN);
        assert_eq!(p.molten, Color::Rgb(232, 82, 26));
    }

    #[test]
    fn tui_color_config_overrides_molten() {
        let cfg = TuiColorConfig {
            molten: Some("#123456".to_owned()),
            ..TuiColorConfig::default()
        };
        let mut p = Palette::default();
        if let Some(ref s) = cfg.molten {
            if let Some(c) = parse_hex(s) {
                p.molten = c;
            }
        }
        assert_eq!(p.molten, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(p.accent, FORGE_ACCENT, "unset accent keeps default");
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
        if let Some(ref s) = cfg.bg {
            if let Some(c) = parse_hex(s) {
                p.bg = c;
            }
        }
        if let Some(ref s) = cfg.border {
            if let Some(c) = parse_hex(s) {
                p.border = c;
            }
        }
        assert_eq!(p.bg, Color::Rgb(255, 0, 0));
        assert_eq!(p.border, Color::Rgb(0, 255, 0));
        assert_eq!(p.text, FORGE_TEXT, "unset slots keep the default");
    }
}
