//! Graceful degradation and tier/status badge resolution.

use crate::registry::GlyphRegistry;

/// Returns `true` when `term` identifies a terminal emulator that supports APC
/// sequences (`WezTerm`, `kitty`, `foot`, `iTerm`).
///
/// The check is case-insensitive and matches on a substring of the `TERM` or
/// `TERM_PROGRAM` environment variable.
#[must_use]
pub fn supports_apc(term: &str) -> bool {
    let lower = term.to_lowercase();
    lower.contains("wezterm")
        || lower.contains("kitty")
        || lower.contains("foot")
        || lower.contains("iterm")
}

/// Returns a plain-text fallback label for terminals that do not render glyphs.
///
/// Unknown glyph IDs map to `"[?]"`.
#[must_use]
pub fn fallback_text(glyph_id: &str) -> &'static str {
    match glyph_id {
        "smedja.tier.local" => "[local]",
        "smedja.tier.fast" => "[fast]",
        "smedja.tier.deep" => "[deep]",
        "smedja.status.ok" => "\u{2713}",      // ✓
        "smedja.status.fail" => "\u{2717}",    // ✗
        "smedja.status.pending" => "\u{23F3}", // ⏳
        "smedja.task" => "[task]",
        _ => "[?]",
    }
}

/// Maps an execution-tier string to its built-in glyph ID.
///
/// Recognises `"local"`, `"fast"`, and `"deep"`; any other value returns
/// `None`.
#[must_use]
pub fn glyph_id_for_tier(tier: &str) -> Option<&'static str> {
    match tier {
        "local" => Some("smedja.tier.local"),
        "fast" => Some("smedja.tier.fast"),
        "deep" => Some("smedja.tier.deep"),
        _ => None,
    }
}

/// Maps a status string to its built-in glyph ID.
///
/// Recognises `"ok"`, `"fail"`, and `"pending"`; any other value returns
/// `None`.
#[must_use]
pub fn glyph_id_for_status(status: &str) -> Option<&'static str> {
    match status {
        "ok" => Some("smedja.status.ok"),
        "fail" => Some("smedja.status.fail"),
        "pending" => Some("smedja.status.pending"),
        _ => None,
    }
}

/// How a tier/status badge should be drawn after resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeRender {
    /// Render the PUA codepoint by sampling its registered glyph bitmap.
    Glyph(char),
    /// Render the plain-text fallback label (APC unsupported or unregistered).
    Text(&'static str),
}

/// Resolves a glyph `id` to either its PUA codepoint or a plain-text fallback.
///
/// Returns [`BadgeRender::Glyph`] with the registered codepoint when `term`
/// supports APC sequences ([`supports_apc`]) **and** `id` is registered in
/// `registry`; otherwise returns [`BadgeRender::Text`] with
/// [`fallback_text(id)`](fallback_text).
#[must_use]
pub fn resolve_badge(registry: &GlyphRegistry, id: &str, term: &str) -> BadgeRender {
    if supports_apc(term) {
        if let Some(cp) = registry.lookup(id) {
            return BadgeRender::Glyph(cp);
        }
    }
    BadgeRender::Text(fallback_text(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::register_builtin_glyphs;

    #[test]
    fn glyph_id_for_tier_maps_known_tiers() {
        assert_eq!(glyph_id_for_tier("local"), Some("smedja.tier.local"));
        assert_eq!(glyph_id_for_tier("fast"), Some("smedja.tier.fast"));
        assert_eq!(glyph_id_for_tier("deep"), Some("smedja.tier.deep"));
    }

    #[test]
    fn glyph_id_for_status_maps_known_statuses() {
        assert_eq!(glyph_id_for_status("ok"), Some("smedja.status.ok"));
        assert_eq!(glyph_id_for_status("fail"), Some("smedja.status.fail"));
        assert_eq!(
            glyph_id_for_status("pending"),
            Some("smedja.status.pending")
        );
    }

    #[test]
    fn glyph_id_for_unknown_returns_none() {
        assert_eq!(glyph_id_for_tier("cloud"), None);
        assert_eq!(glyph_id_for_status("unknown"), None);
    }

    #[test]
    fn badge_resolves_to_codepoint_when_registered_and_apc_supported() {
        let mut reg = GlyphRegistry::new();
        register_builtin_glyphs(&mut reg);
        let resolved = resolve_badge(&reg, "smedja.tier.deep", "xterm-wezterm");
        assert_eq!(
            resolved,
            BadgeRender::Glyph(reg.lookup("smedja.tier.deep").expect("deep registered"))
        );
    }

    #[test]
    fn badge_falls_back_to_text_when_apc_unsupported() {
        let mut reg = GlyphRegistry::new();
        register_builtin_glyphs(&mut reg);
        let resolved = resolve_badge(&reg, "smedja.tier.deep", "xterm-256color");
        assert_eq!(
            resolved,
            BadgeRender::Text(fallback_text("smedja.tier.deep"))
        );
    }

    #[test]
    fn badge_falls_back_to_text_when_unregistered() {
        let reg = GlyphRegistry::new(); // no built-ins registered
        let resolved = resolve_badge(&reg, "smedja.tier.deep", "xterm-wezterm");
        assert_eq!(
            resolved,
            BadgeRender::Text(fallback_text("smedja.tier.deep"))
        );
    }

    #[test]
    fn supports_apc_wezterm_returns_true() {
        assert!(supports_apc("xterm-256color-wezterm"));
    }

    #[test]
    fn supports_apc_xterm_returns_false() {
        assert!(!supports_apc("xterm-256color"));
    }

    #[test]
    fn fallback_text_maps_tier_local() {
        assert_eq!(fallback_text("smedja.tier.local"), "[local]");
    }
}
