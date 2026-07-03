//! Cell, block-decoration, and agent-block view types.

/// A single terminal cell to be rendered.
///
/// `fg`/`bg` are already resolved by the caller (inverse-video swap and dim
/// scaling are applied upstream in the bridge), so the renderer only needs the
/// glyph-shaping flags here: bold/italic pick the font variant, `wide` centres a
/// double-width glyph over two columns, and underline/strikethrough draw rules.
#[derive(Debug, Clone, PartialEq, Default)]
#[allow(clippy::struct_excessive_bools)]
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
    /// Use the bold font variant.
    pub bold: bool,
    /// Use the italic font variant.
    pub italic: bool,
    /// Draw an underline rule.
    pub underline: bool,
    /// Draw a strikethrough rule.
    pub strikethrough: bool,
    /// Leading cell of a double-width glyph (centre over two columns).
    pub wide: bool,
}

/// A decorative overlay drawn over a block span.
#[derive(Debug, Clone)]
pub struct BlockDecoration {
    /// First row of the block.
    pub start_row: u16,
    /// Last row of the block (inclusive).
    pub end_row: u16,
    /// Exit code, used to determine colour of the badge.
    pub exit_code: Option<i32>,
    /// Whether this block is currently selected.
    pub selected: bool,
}

/// An agent block for rendering.
#[derive(Debug, Clone)]
pub struct AgentBlockView {
    /// Start row in the terminal grid.
    pub start_row: u16,
    /// Model name displayed in the header.
    pub model: String,
    /// Streamed content lines.
    pub content_lines: Vec<String>,
    /// Whether an approval prompt is visible.
    pub approval_pending: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_is_plain_old_data() {
        // Verify Cell fields are accessible.
        let c = Cell {
            ch: 'A',
            fg: [1.0, 1.0, 1.0, 1.0],
            bg: [0.0, 0.0, 0.0, 1.0],
            col: 5,
            row: 3,
            ..Cell::default()
        };
        assert_eq!(c.ch, 'A');
        assert_eq!(c.col, 5);
    }

    #[test]
    fn block_decoration_fields_accessible() {
        let d = BlockDecoration {
            start_row: 0,
            end_row: 5,
            exit_code: Some(0),
            selected: false,
        };
        assert_eq!(d.end_row, 5);
    }
}
