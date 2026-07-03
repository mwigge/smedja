//! Per-cell style flags and the terminal [`Cell`] type.

use crate::color::{DEFAULT_BG, DEFAULT_FG};

/// Per-cell style and layout flags (bitset).
///
/// `WIDE` marks the leading cell of a double-width glyph (CJK/emoji); the cell
/// to its right is a `WIDE_SPACER` placeholder the renderer skips. The rest are
/// SGR style attributes carried per cell so the renderer can apply them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellFlags(u16);

impl CellFlags {
    /// Leading cell of a 2-column (double-width) glyph.
    pub const WIDE: Self = Self(1 << 0);
    /// Trailing placeholder cell after a `WIDE` glyph (not drawn).
    pub const WIDE_SPACER: Self = Self(1 << 1);
    /// SGR 1 — bold.
    pub const BOLD: Self = Self(1 << 2);
    /// SGR 3 — italic.
    pub const ITALIC: Self = Self(1 << 3);
    /// SGR 4 — underline.
    pub const UNDERLINE: Self = Self(1 << 4);
    /// SGR 9 — strikethrough.
    pub const STRIKETHROUGH: Self = Self(1 << 5);
    /// SGR 2 — dim/faint.
    pub const DIM: Self = Self(1 << 6);
    /// SGR 7 — reverse video (swap fg/bg).
    pub const INVERSE: Self = Self(1 << 7);

    /// The empty flag set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Returns `true` when every bit in `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Sets the bits in `other`.
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

impl std::ops::BitOr for CellFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for CellFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// A single terminal cell.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    /// The Unicode scalar displayed in this cell.
    pub ch: char,
    /// Foreground colour as linear RGBA.
    pub fg: [f32; 4],
    /// Background colour as linear RGBA.
    pub bg: [f32; 4],
    /// Column index (0-based).
    pub col: u16,
    /// Row index (0-based).
    pub row: u16,
    /// OSC 8 hyperlink URI, if any.
    pub url: Option<String>,
    /// Style + layout flags ([`CellFlags`]).
    pub flags: CellFlags,
}

impl Cell {
    /// Creates a blank space cell with default colours.
    #[must_use]
    pub fn blank(col: u16, row: u16) -> Self {
        Self {
            ch: ' ',
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            col,
            row,
            url: None,
            flags: CellFlags::empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_blank_has_space_char() {
        let c = Cell::blank(0, 0);
        assert_eq!(c.ch, ' ');
    }
}
