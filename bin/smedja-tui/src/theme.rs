//! Theme colour map for smedja TUI agent states.

use std::collections::HashMap;

use crossterm::style::Color;

/// Returns the canonical agent theme colour map.
///
/// Keys: `"local"`, `"fast"`, `"deep"`, `"error"`, `"success"`, `"warn"`,
/// `"border"`, `"highlight"`.
#[must_use]
pub fn agent_theme() -> HashMap<&'static str, Color> {
    let mut m = HashMap::new();
    m.insert("local", Color::Cyan);
    m.insert("fast", Color::Blue);
    m.insert("deep", Color::Magenta);
    m.insert("error", Color::Red);
    m.insert("success", Color::Green);
    m.insert("warn", Color::Yellow);
    m.insert("border", Color::DarkGrey);
    m.insert("highlight", Color::White);
    m
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
    fn colours_are_deterministic() {
        assert_eq!(agent_theme().get("error"), Some(&Color::Red));
        assert_eq!(agent_theme().get("success"), Some(&Color::Green));
    }
}
