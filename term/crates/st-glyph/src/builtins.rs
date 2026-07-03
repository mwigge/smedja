//! Built-in smedja glyph SVG sources and their registration.

use crate::registry::GlyphRegistry;

/// SVG source for the `smedja.tier.local` glyph (circle).
const SVG_TIER_LOCAL: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><circle cx="8" cy="8" r="7" fill="#4a9eff"/></svg>"##;

/// SVG source for the `smedja.tier.fast` glyph (lightning bolt).
const SVG_TIER_FAST: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><polygon points="9,1 4,9 8,9 7,15 12,7 8,7" fill="#ffcc00"/></svg>"##;

/// SVG source for the `smedja.tier.deep` glyph (brain/cloud).
const SVG_TIER_DEEP: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><ellipse cx="8" cy="7" rx="6" ry="5" fill="#9b59b6"/><rect x="5" y="10" width="6" height="3" rx="1" fill="#9b59b6"/></svg>"##;

/// SVG source for the `smedja.status.ok` glyph (checkmark).
const SVG_STATUS_OK: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><polyline points="2,8 6,12 14,4" stroke="#2ecc71" stroke-width="2.5" fill="none"/></svg>"##;

/// SVG source for the `smedja.status.fail` glyph (X mark).
const SVG_STATUS_FAIL: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><line x1="3" y1="3" x2="13" y2="13" stroke="#e74c3c" stroke-width="2.5"/><line x1="13" y1="3" x2="3" y2="13" stroke="#e74c3c" stroke-width="2.5"/></svg>"##;

/// SVG source for the `smedja.status.pending` glyph (hourglass).
const SVG_STATUS_PENDING: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><polygon points="3,1 13,1 8,8 13,15 3,15 8,8" fill="#f39c12"/></svg>"##;

/// SVG source for the `smedja.task` glyph (clipboard).
const SVG_TASK: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><rect x="2" y="3" width="12" height="12" rx="1" fill="#95a5a6"/><rect x="5" y="1" width="6" height="3" rx="1" fill="#7f8c8d"/></svg>"##;

/// Mapping of built-in glyph IDs to their SVG source strings.
pub static BUILTIN_GLYPHS: &[(&str, &str)] = &[
    ("smedja.tier.local", SVG_TIER_LOCAL),
    ("smedja.tier.fast", SVG_TIER_FAST),
    ("smedja.tier.deep", SVG_TIER_DEEP),
    ("smedja.status.ok", SVG_STATUS_OK),
    ("smedja.status.fail", SVG_STATUS_FAIL),
    ("smedja.status.pending", SVG_STATUS_PENDING),
    ("smedja.task", SVG_TASK),
];

/// Registers all built-in smedja glyphs in `registry`.
///
/// After this call every glyph ID in [`BUILTIN_GLYPHS`] has a PUA codepoint
/// that can be retrieved via [`GlyphRegistry::lookup`].
pub fn register_builtin_glyphs(registry: &mut GlyphRegistry) {
    for (id, _svg) in BUILTIN_GLYPHS {
        registry.register(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_glyphs_all_register_without_panic() {
        let mut reg = GlyphRegistry::new();
        register_builtin_glyphs(&mut reg);
        for (id, _) in BUILTIN_GLYPHS {
            assert!(
                reg.lookup(id).is_some(),
                "expected glyph '{id}' to be registered"
            );
        }
    }
}
