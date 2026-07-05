//! Terminal colour types and the default ANSI palette.

// ── Colour types ──────────────────────────────────────────────────────────────

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

/// Default ANSI palette (forged_terminal).
pub(crate) const DEFAULT_PALETTE: [[f32; 4]; 16] = [
    [0.067, 0.075, 0.086, 1.0], // 0  #111316
    [0.839, 0.373, 0.180, 1.0], // 1  #d65f2e
    [0.365, 0.580, 0.420, 1.0], // 2  #5d946b
    [0.851, 0.608, 0.333, 1.0], // 3  #d99b55
    [0.561, 0.463, 0.357, 1.0], // 4  #8f765b
    [0.663, 0.396, 0.184, 1.0], // 5  #a9652f
    [0.969, 0.780, 0.494, 1.0], // 6  #f7c77e
    [0.957, 0.843, 0.631, 1.0], // 7  #f4d7a1
    [0.231, 0.165, 0.122, 1.0], // 8  #3b2a1f
    [0.910, 0.459, 0.243, 1.0], // 9  #e8753e
    [0.467, 0.667, 0.486, 1.0], // 10 #77aa7c
    [1.000, 0.827, 0.478, 1.0], // 11 #ffd37a
    [0.706, 0.518, 0.353, 1.0], // 12 #b4845a
    [0.753, 0.478, 0.227, 1.0], // 13 #c07a3a
    [1.000, 0.698, 0.290, 1.0], // 14 #ffb24a
    [1.000, 0.945, 0.812, 1.0], // 15 #fff1cf
];
