//! Keyboard-driven copy mode: selection, clipboard copy, and search.

use crate::error::PtyError;
use crate::grid::CellGrid;

/// Copy mode state for keyboard-driven selection.
#[derive(Debug, Default)]
pub struct CopyMode {
    /// Whether copy mode is active.
    pub active: bool,
    /// Current cursor position in copy mode `(col, row)`.
    pub cursor: (u16, u16),
    /// Visual selection anchor `(col, row)`, if any.
    pub anchor: Option<(u16, u16)>,
    /// Current search query.
    pub search_query: String,
    /// Matching cell positions `(col, row)`.
    pub search_matches: Vec<(u16, u16)>,
}

impl CopyMode {
    /// Creates a new [`CopyMode`] anchored at `(0, 0)`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enters copy mode, positioning the copy cursor at the current terminal
    /// cursor position.
    pub fn enter(&mut self, terminal_cursor: (u16, u16)) {
        self.active = true;
        self.cursor = terminal_cursor;
        self.anchor = None;
        self.search_matches.clear();
    }

    /// Exits copy mode.
    pub fn exit(&mut self) {
        self.active = false;
        self.anchor = None;
        self.search_matches.clear();
    }

    /// Copies the selected region of `grid` to the system clipboard.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Clipboard`] if the clipboard cannot be opened or
    /// written to.
    pub fn copy_selection(&mut self, grid: &CellGrid) -> Result<(), PtyError> {
        let Some(anchor) = self.anchor else {
            return Ok(());
        };
        let (ac, ar) = anchor;
        let (cc, cr) = self.cursor;

        // Normalise selection bounds.
        let (r_start, r_end) = if ar <= cr { (ar, cr) } else { (cr, ar) };
        let (c_start, c_end) = if ar == cr {
            if ac <= cc {
                (ac, cc)
            } else {
                (cc, ac)
            }
        } else {
            (0, grid.cols.saturating_sub(1))
        };

        let mut text = String::new();
        for row in r_start..=r_end {
            let start_col = if row == r_start { c_start } else { 0 };
            let end_col = if row == r_end {
                c_end
            } else {
                grid.cols.saturating_sub(1)
            };

            if let Some(r) = grid.cells.get(row as usize) {
                for col in start_col..=end_col {
                    if let Some(cell) = r.get(col as usize) {
                        text.push(cell.ch);
                    }
                }
            }
            if row < r_end {
                text.push('\n');
            }
        }

        let mut clipboard =
            arboard::Clipboard::new().map_err(|e| PtyError::Clipboard(e.to_string()))?;
        clipboard
            .set_text(text)
            .map_err(|e| PtyError::Clipboard(e.to_string()))?;
        Ok(())
    }

    /// Searches for `query` in the grid and populates `search_matches`.
    pub fn search(&mut self, query: &str, grid: &CellGrid) {
        self.search_query = query.to_owned();
        self.search_matches.clear();

        if query.is_empty() {
            return;
        }

        // Build a flat string per row and search for matches.
        for (r, row) in grid.cells.iter().enumerate() {
            let line: String = row.iter().map(|c| c.ch).collect();
            let mut start = 0;
            while let Some(pos) = line[start..].find(query) {
                let abs = start + pos;
                self.search_matches.push((abs as u16, r as u16));
                start = abs + 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_grid(cols: u16, rows: u16) -> CellGrid {
        CellGrid::new(cols, rows)
    }

    #[test]
    fn copy_mode_search_finds_matches() {
        let mut grid = make_grid(20, 5);
        // Write "hello" into row 0
        for (i, ch) in "hello world".chars().enumerate() {
            grid.cells[0][i].ch = ch;
        }
        let mut cm = CopyMode::new();
        cm.search("hello", &grid);
        assert!(!cm.search_matches.is_empty());
        assert_eq!(cm.search_matches[0], (0, 0));
    }

    #[test]
    fn copy_mode_search_empty_query_clears_matches() {
        let grid = make_grid(20, 5);
        let mut cm = CopyMode::new();
        cm.search_matches.push((0, 0));
        cm.search("", &grid);
        assert!(cm.search_matches.is_empty());
    }
}
