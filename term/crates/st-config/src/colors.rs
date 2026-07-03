//! Colour palette type, hex parsing, and the built-in `forged_terminal` theme.

use serde::{Deserialize, Serialize};

use crate::ConfigError;

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
pub(crate) fn forged_terminal_colors() -> ColorConfig {
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
}
