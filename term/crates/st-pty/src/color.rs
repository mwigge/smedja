//! Terminal colour values and palette resolution.

pub(crate) const DEFAULT_FG: [f32; 4] = [0.957, 0.843, 0.631, 1.0]; // #f4d7a1
pub(crate) const DEFAULT_BG: [f32; 4] = [0.043, 0.051, 0.059, 1.0]; // #0b0d0f

/// A terminal colour value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Color {
    /// Use the cell's default colour.
    Default,
    /// One of the 16 ANSI palette colours (0-15).
    Ansi(u8),
    /// 256-colour palette entry (0-255).
    Ansi256(u8),
    /// 24-bit RGB colour.
    Rgb(u8, u8, u8),
}

impl Color {
    /// Resolves the colour to a linear RGBA value given the ANSI palette.
    #[must_use]
    pub fn to_rgba(&self, palette: &[[f32; 4]; 16], is_fg: bool) -> [f32; 4] {
        match self {
            Self::Default => {
                if is_fg {
                    DEFAULT_FG
                } else {
                    DEFAULT_BG
                }
            }
            Self::Ansi(n) => {
                let idx = usize::from(*n).min(15);
                palette[idx]
            }
            Self::Ansi256(n) => ansi256_to_rgba(*n),
            Self::Rgb(r, g, b) => [
                f32::from(*r) / 255.0,
                f32::from(*g) / 255.0,
                f32::from(*b) / 255.0,
                1.0,
            ],
        }
    }
}

/// Converts a 256-colour palette index to RGBA.
#[must_use]
fn ansi256_to_rgba(n: u8) -> [f32; 4] {
    match n {
        0..=15 => {
            // Standard ANSI colours — use simple defaults for now.
            [
                f32::from(n & 1) * if n >= 8 { 1.0 } else { 0.8 },
                f32::from((n >> 1) & 1) * if n >= 8 { 1.0 } else { 0.8 },
                f32::from((n >> 2) & 1) * if n >= 8 { 1.0 } else { 0.8 },
                1.0,
            ]
        }
        16..=231 => {
            // 6×6×6 colour cube
            let v = u32::from(n) - 16;
            let b = (v % 6) * 51;
            let g = ((v / 6) % 6) * 51;
            let r = (v / 36) * 51;
            [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
        }
        232..=255 => {
            // Greyscale ramp
            let grey = (u32::from(n) - 232) * 10 + 8;
            let v = grey as f32 / 255.0;
            [v, v, v, 1.0]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::DEFAULT_PALETTE;

    #[test]
    fn color_to_rgba_ansi_uses_palette() {
        let palette = DEFAULT_PALETTE;
        let rgba = Color::Ansi(0).to_rgba(&palette, true);
        assert_eq!(rgba, palette[0]);
    }

    #[test]
    fn color_to_rgba_rgb_scales_correctly() {
        let palette = DEFAULT_PALETTE;
        let rgba = Color::Rgb(255, 0, 0).to_rgba(&palette, true);
        assert!((rgba[0] - 1.0).abs() < 1e-6);
        assert_eq!(rgba[1], 0.0);
        assert_eq!(rgba[2], 0.0);
    }
}
