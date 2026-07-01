pub(crate) fn status_bar_height_for_font(font_size: f32) -> u32 {
    // Clamp to zero before truncating so negative font sizes don't wrap.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let px = font_size.max(0.0) as u32;
    px.min(36)
}

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
