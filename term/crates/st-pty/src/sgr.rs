//! Current SGR (Select Graphic Rendition) attribute state.

use crate::cell::CellFlags;
use crate::color::Color;

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
