//! Rendering glue: cell conversion, status-bar/title helpers, clipboard, icon.
//!
//! Helper functions that bridge PTY/agent state to the renderer and window:
//! converting grid cells to renderer cells, resolving tier badges, building the
//! window title, reading the clipboard, and loading the window icon.

// ── Status bar height ──────────────────────────────────────────────────────────

/// Returns the status bar height in pixels for a given font size.
///
/// Mirrors the formula in `st_render::Renderer::status_bar_height_px` so that
/// callers without a renderer can compute the same value.
pub(crate) fn status_bar_height_for_font(font_size: f32) -> u32 {
    // Clamp to zero before truncating so negative font sizes don't wrap.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let px = font_size.max(0.0) as u32;
    px.min(36)
}

// ── Tier badge resolution ──────────────────────────────────────────────────────

/// Resolves a status-bar tier badge to its display text.
///
/// Maps `tier` to its built-in glyph ID via [`st_glyph::glyph_id_for_tier`],
/// then resolves it against `registry` and `term`: when the terminal supports
/// APC sequences and the glyph is registered, returns the single PUA codepoint
/// as a `String`; otherwise returns the plain-text fallback (e.g. `[deep]`).
///
/// An unknown tier returns `[<tier>]` unchanged so existing behaviour is kept.
#[must_use]
pub(crate) fn tier_badge_text(
    registry: &st_glyph::GlyphRegistry,
    tier: &str,
    term: &str,
) -> String {
    let Some(glyph_id) = st_glyph::glyph_id_for_tier(tier) else {
        return format!("[{tier}]");
    };
    match st_glyph::resolve_badge(registry, glyph_id, term) {
        st_glyph::BadgeRender::Glyph(cp) => cp.to_string(),
        st_glyph::BadgeRender::Text(text) => text.to_owned(),
    }
}

// ── Window title helpers ───────────────────────────────────────────────────────

#[must_use]
pub(crate) fn build_window_title(
    tier: Option<&str>,
    mode: Option<&str>,
    session_id: Option<&str>,
    cwd: Option<&str>,
) -> String {
    let mut parts = vec!["smedja".to_owned()];
    if let Some(t) = tier {
        parts.push(format!("[{t}]"));
    }
    if let Some(m) = mode {
        parts.push(format!("[{m}]"));
    }
    if let Some(s) = session_id {
        parts.push(s[..s.len().min(8)].to_owned());
    }
    if let Some(c) = cwd {
        parts.push(truncate_cwd(c, 40));
    }
    parts.join("  ")
}

#[must_use]
pub(crate) fn truncate_cwd(cwd: &str, max: usize) -> String {
    let n = cwd.chars().count();
    if n <= max {
        return cwd.to_owned();
    }
    // Byte offset of the start of the last `max` chars — always a char boundary,
    // so a multibyte path component can't panic the slice (a raw `cwd.len()-max`
    // byte index can land mid-UTF-8).
    let start = cwd.char_indices().nth(n - max).map_or(0, |(i, _)| i);
    format!("\u{2026}{}", &cwd[start..])
}

// ── Clipboard ──────────────────────────────────────────────────────────────────

/// Reads the system clipboard as text, robustly across environments.
///
/// Prefers the platform CLI tools — `wl-paste` (Wayland), `xclip`/`xsel` (X11),
/// `pbpaste` (macOS) — because `arboard`'s Wayland backend is unreliable and
/// silently returned nothing, so Ctrl+V pasted nothing into apps like
/// smedja-tui. Falls back to `arboard` when no CLI tool is available.
pub(crate) fn read_clipboard_text() -> Option<String> {
    use std::process::Command;
    let try_cmd = |cmd: &str, args: &[&str]| -> Option<String> {
        let out = Command::new(cmd).args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };

    #[cfg(target_os = "macos")]
    if let Some(t) = try_cmd("pbpaste", &[]) {
        return Some(t);
    }
    #[cfg(not(target_os = "macos"))]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            if let Some(t) = try_cmd("wl-paste", &["--no-newline"]) {
                return Some(t);
            }
        }
        if let Some(t) = try_cmd("xclip", &["-selection", "clipboard", "-o"]) {
            return Some(t);
        }
        if let Some(t) = try_cmd("xsel", &["-b", "-o"]) {
            return Some(t);
        }
    }
    // Last resort: arboard (works on most setups, flaky on some Wayland ones).
    arboard::Clipboard::new().ok()?.get_text().ok()
}

// ── Cell conversion ────────────────────────────────────────────────────────────

/// Converts a PTY grid cell into a renderer cell at `(col, row)`, resolving the
/// style flags: inverse swaps fg/bg, dim scales the foreground, and the
/// remaining flags (bold/italic/underline/strikethrough/wide) pass through for
/// the renderer to apply.
pub(crate) fn render_cell(c: &st_pty::Cell, col: u16, row: u16) -> st_render::Cell {
    use st_pty::CellFlags;
    let f = c.flags;
    let (mut fg, mut bg) = (c.fg, c.bg);
    if f.contains(CellFlags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if f.contains(CellFlags::DIM) {
        for ch in fg.iter_mut().take(3) {
            *ch *= 0.6;
        }
    }
    st_render::Cell {
        ch: c.ch,
        fg,
        bg,
        col,
        row,
        bold: f.contains(CellFlags::BOLD),
        italic: f.contains(CellFlags::ITALIC),
        underline: f.contains(CellFlags::UNDERLINE),
        strikethrough: f.contains(CellFlags::STRIKETHROUGH),
        wide: f.contains(CellFlags::WIDE),
    }
}

// ── Window icon ───────────────────────────────────────────────────────────────

/// Loads the smedja brand icon from the embedded PNG and returns a winit `Icon`.
///
/// Only called on Linux; macOS uses the `.icns` bundle resource. Returns `None`
/// on decode or icon-creation failure so the caller can skip silently.
pub(crate) fn load_window_icon() -> Option<winit::window::Icon> {
    let png_bytes = include_bytes!("../../../../assets/brand/smedja-256.png");
    let img = image::load_from_memory(png_bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    winit::window::Icon::from_rgba(img.into_raw(), w, h).ok()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{build_window_title, tier_badge_text, truncate_cwd};

    #[test]
    fn tier_badge_resolves_to_pua_glyph_when_apc_supported() {
        let mut reg = st_glyph::GlyphRegistry::new();
        st_glyph::register_builtin_glyphs(&mut reg);
        let badge = tier_badge_text(&reg, "deep", "xterm-wezterm");
        let cp = reg.lookup("smedja.tier.deep").expect("deep is registered");
        assert_eq!(badge, cp.to_string());
    }

    #[test]
    fn tier_badge_falls_back_to_text_when_apc_unsupported() {
        let mut reg = st_glyph::GlyphRegistry::new();
        st_glyph::register_builtin_glyphs(&mut reg);
        let badge = tier_badge_text(&reg, "deep", "xterm-256color");
        assert_eq!(badge, "[deep]");
    }

    #[test]
    fn tier_badge_unknown_tier_is_unchanged() {
        let reg = st_glyph::GlyphRegistry::new();
        assert_eq!(tier_badge_text(&reg, "cloud", "xterm-wezterm"), "[cloud]");
    }

    #[test]
    fn build_window_title_contains_required_parts() {
        let title = build_window_title(
            Some("fast"),
            Some("impl"),
            Some("abc12345xyz"),
            Some("/home/u/proj"),
        );
        assert!(title.contains("smedja"), "must contain app name");
        assert!(title.contains("[fast]"), "must contain tier");
        assert!(title.contains("[impl]"), "must contain mode");
        assert!(
            title.contains("abc12345"),
            "must contain first 8 chars of session_id"
        );
        assert!(title.contains("/home/u/proj"), "must contain cwd");
    }

    #[test]
    fn build_window_title_all_none_returns_smedja() {
        let title = build_window_title(None, None, None, None);
        assert_eq!(title, "smedja");
    }

    #[test]
    fn truncate_cwd_long_path_starts_with_ellipsis() {
        let long = "/very/long/path/that/exceeds/limit";
        let result = truncate_cwd(long, 10);
        assert!(
            result.starts_with('\u{2026}'),
            "expected … prefix, got '{result}'"
        );
        assert!(
            result.chars().count() <= 11,
            "result must be at most max+1 chars"
        );
    }

    #[test]
    fn truncate_cwd_short_path_unchanged() {
        let short = "/short";
        let result = truncate_cwd(short, 40);
        assert_eq!(result, "/short");
    }

    #[test]
    fn truncate_cwd_multibyte_does_not_panic_on_boundary() {
        // A path whose tail boundary falls inside multibyte chars: a raw byte
        // slice would panic here. Each `é` is 2 bytes, `ä` 2, `语` 3.
        let path = "/home/josé/projets/café/编程语言/source";
        let result = truncate_cwd(path, 10);
        assert!(result.starts_with('\u{2026}'));
        assert_eq!(result.chars().count(), 11, "ellipsis + last 10 chars");
    }
}
