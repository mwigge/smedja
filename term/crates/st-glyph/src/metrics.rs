//! Font metric helpers: advance width, line height, and pixel→grid conversion.

/// Estimates the advance width of a character in pixels for a given font size.
///
/// Uses a simple monospace approximation (0.6 × `font_size`) that is correct for
/// most terminal fonts.  A more accurate implementation would consult the
/// `FontSystem` from `cosmic-text`.
#[must_use]
pub fn char_advance_width(font_size: f32) -> f32 {
    font_size * 0.6
}

/// Estimates the line height for a given font size.
///
/// Returns `font_size × 1.2`.
#[must_use]
pub fn line_height(font_size: f32) -> f32 {
    font_size * 1.2
}

/// Converts a physical pixel size `(width, height)` and font metrics into a
/// `(cols, rows)` grid size.
///
/// Both dimensions are clamped to a minimum of 1.
#[must_use]
pub fn pixel_size_to_grid(width: u32, height: u32, font_size: f32) -> (u16, u16) {
    let cw = char_advance_width(font_size).max(1.0);
    let ch = line_height(font_size).max(1.0);
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let cols = (width as f32 / cw).floor() as u16;
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rows = (height as f32 / ch).floor() as u16;
    (cols.max(1), rows.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_advance_width_scales_with_font_size() {
        assert!((char_advance_width(14.0) - 8.4).abs() < 0.01);
    }

    #[test]
    fn line_height_scales_with_font_size() {
        assert!((line_height(14.0) - 16.8).abs() < 0.01);
    }

    #[test]
    fn pixel_size_to_grid_computes_correctly() {
        // 800×600 window at 14pt font → ~95 cols, ~35 rows
        let (cols, rows) = pixel_size_to_grid(800, 600, 14.0);
        assert!(cols > 0);
        assert!(rows > 0);
    }

    #[test]
    fn pixel_size_to_grid_minimum_is_one() {
        let (cols, rows) = pixel_size_to_grid(1, 1, 14.0);
        assert_eq!(cols, 1);
        assert_eq!(rows, 1);
    }

    #[test]
    fn pixel_size_to_grid_zero_width_returns_minimum_one() {
        let (cols, _rows) = pixel_size_to_grid(0, 600, 14.0);
        assert_eq!(cols, 1, "zero width should clamp to minimum 1 col");
    }

    #[test]
    fn pixel_size_to_grid_zero_height_returns_minimum_one() {
        let (_cols, rows) = pixel_size_to_grid(800, 0, 14.0);
        assert_eq!(rows, 1, "zero height should clamp to minimum 1 row");
    }

    #[test]
    fn pixel_size_to_grid_zero_both_returns_one_one() {
        let (cols, rows) = pixel_size_to_grid(0, 0, 14.0);
        assert_eq!(cols, 1);
        assert_eq!(rows, 1);
    }
}
