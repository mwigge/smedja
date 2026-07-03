//! SGR (Select Graphic Rendition) attribute state and parameter parsing.

use tracing::debug;

use crate::cell::CellFlags;
use crate::color::Color;
use crate::grid::CellGrid;

/// Current SGR (Select Graphic Rendition) attribute state.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct SgrState {
    pub(crate) fg: Color,
    pub(crate) bg: Color,
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) underline: bool,
    pub(crate) strikethrough: bool,
    pub(crate) dim: bool,
    pub(crate) inverse: bool,
    /// OSC 8 URL currently in scope.
    pub(crate) url: Option<String>,
}

impl Default for SgrState {
    fn default() -> Self {
        Self {
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            dim: false,
            inverse: false,
            url: None,
        }
    }
}

impl SgrState {
    /// Builds the per-cell [`CellFlags`] for the current style attributes.
    pub(crate) fn cell_flags(&self) -> CellFlags {
        let mut f = CellFlags::empty();
        if self.bold {
            f |= CellFlags::BOLD;
        }
        if self.italic {
            f |= CellFlags::ITALIC;
        }
        if self.underline {
            f |= CellFlags::UNDERLINE;
        }
        if self.strikethrough {
            f |= CellFlags::STRIKETHROUGH;
        }
        if self.dim {
            f |= CellFlags::DIM;
        }
        if self.inverse {
            f |= CellFlags::INVERSE;
        }
        f
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Applies SGR parameters to the grid's current SGR state.
pub(crate) fn apply_sgr(grid: &mut CellGrid, params: &[u16]) {
    let mut i = 0;
    if params.is_empty() {
        grid.sgr.reset();
        return;
    }
    while i < params.len() {
        match params[i] {
            0 => grid.sgr.reset(),
            1 => grid.sgr.bold = true,
            2 => grid.sgr.dim = true,
            3 => grid.sgr.italic = true,
            4 => grid.sgr.underline = true,
            7 => grid.sgr.inverse = true,
            9 => grid.sgr.strikethrough = true,
            22 => {
                grid.sgr.bold = false;
                grid.sgr.dim = false;
            }
            23 => grid.sgr.italic = false,
            24 => grid.sgr.underline = false,
            27 => grid.sgr.inverse = false,
            29 => grid.sgr.strikethrough = false,
            // Standard fg colours 30-37, bright fg 90-97.
            n @ 30..=37 => grid.sgr.fg = Color::Ansi((n - 30) as u8),
            39 => grid.sgr.fg = Color::Default,
            n @ 40..=47 => grid.sgr.bg = Color::Ansi((n - 40) as u8),
            49 => grid.sgr.bg = Color::Default,
            n @ 90..=97 => grid.sgr.fg = Color::Ansi((n - 90 + 8) as u8),
            n @ 100..=107 => grid.sgr.bg = Color::Ansi((n - 100 + 8) as u8),
            // 256-colour: 38;5;n (fg) / 48;5;n (bg)
            38 if params.get(i + 1) == Some(&5) => {
                if let Some(&n) = params.get(i + 2) {
                    grid.sgr.fg = Color::Ansi256(n as u8);
                    i += 2;
                }
            }
            48 if params.get(i + 1) == Some(&5) => {
                if let Some(&n) = params.get(i + 2) {
                    grid.sgr.bg = Color::Ansi256(n as u8);
                    i += 2;
                }
            }
            // 24-bit: 38;2;r;g;b (fg) / 48;2;r;g;b (bg)
            38 if params.get(i + 1) == Some(&2) => {
                if let (Some(&r), Some(&g), Some(&b)) =
                    (params.get(i + 2), params.get(i + 3), params.get(i + 4))
                {
                    grid.sgr.fg = Color::Rgb(r as u8, g as u8, b as u8);
                    i += 4;
                }
            }
            48 if params.get(i + 1) == Some(&2) => {
                if let (Some(&r), Some(&g), Some(&b)) =
                    (params.get(i + 2), params.get(i + 3), params.get(i + 4))
                {
                    grid.sgr.bg = Color::Rgb(r as u8, g as u8, b as u8);
                    i += 4;
                }
            }
            n => {
                debug!("unhandled SGR param: {}", n);
            }
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::CellGrid;

    fn make_grid(cols: u16, rows: u16) -> CellGrid {
        CellGrid::new(cols, rows)
    }

    #[test]
    fn apply_sgr_reset_clears_state() {
        let mut grid = make_grid(4, 4);
        grid.sgr.bold = true;
        grid.sgr.fg = Color::Ansi(1);
        apply_sgr(&mut grid, &[0]);
        assert!(!grid.sgr.bold);
        assert_eq!(grid.sgr.fg, Color::Default);
    }

    #[test]
    fn apply_sgr_sets_256_fg() {
        let mut grid = make_grid(4, 4);
        apply_sgr(&mut grid, &[38, 5, 200]);
        assert_eq!(grid.sgr.fg, Color::Ansi256(200));
    }

    #[test]
    fn apply_sgr_sets_rgb_bg() {
        let mut grid = make_grid(4, 4);
        apply_sgr(&mut grid, &[48, 2, 10, 20, 30]);
        assert_eq!(grid.sgr.bg, Color::Rgb(10, 20, 30));
    }
}
