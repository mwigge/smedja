//! Screen-mutating operations on [`CellGrid`]: character placement, scrolling,
//! erasing, alternate-screen switching, and cursor movement.

use unicode_width::UnicodeWidthChar;

use crate::cell::{Cell, CellFlags};
use crate::grid::{blank_grid, blank_row, CellGrid};
use crate::marker::{BlockMarker, MarkerKind};

impl CellGrid {
    fn current_fg(&self) -> [f32; 4] {
        self.sgr.fg.to_rgba(&self.palette, true)
    }

    fn current_bg(&self) -> [f32; 4] {
        self.sgr.bg.to_rgba(&self.palette, false)
    }

    #[allow(dead_code)] // reserved for future use in cursor rendering
    fn cell_at_cursor_mut(&mut self) -> Option<&mut Cell> {
        let (col, row) = self.cursor;
        self.cells
            .get_mut(row as usize)
            .and_then(|r| r.get_mut(col as usize))
    }

    pub(crate) fn put_char(&mut self, ch: char) {
        // Consume a deferred wrap from the previous last-column write before
        // placing this glyph (xterm last-column behaviour).
        if self.pending_wrap {
            self.pending_wrap = false;
            self.cursor.0 = 0;
            self.advance_row();
        }

        // Display width: 2 for CJK/emoji, 1 otherwise. Zero-width (combining)
        // marks are treated as width 1 for now — proper grapheme combining is
        // deferred — and anything wider is clamped to 2.
        let w: u16 = match UnicodeWidthChar::width(ch) {
            Some(2) => 2,
            _ => 1,
        };

        let (mut col, mut row) = self.cursor;
        // A double-width glyph that won't fit in the final column wraps to the
        // next line, leaving the last column blank (standard VT behaviour).
        if w == 2 && col + 1 >= self.cols {
            self.cursor.0 = 0;
            self.advance_row();
            col = self.cursor.0;
            row = self.cursor.1;
        }

        let fg = self.current_fg();
        let bg = self.current_bg();
        let url = self.sgr.url.clone();
        let mut flags = self.sgr.cell_flags();
        if w == 2 {
            flags |= CellFlags::WIDE;
        }
        if let Some(cell) = self
            .cells
            .get_mut(row as usize)
            .and_then(|r| r.get_mut(col as usize))
        {
            *cell = Cell {
                ch,
                fg,
                bg,
                col,
                row,
                url: url.clone(),
                flags,
            };
        }
        // The trailing half of a wide glyph is a non-drawn spacer cell.
        if w == 2 {
            let scol = col + 1;
            if let Some(cell) = self
                .cells
                .get_mut(row as usize)
                .and_then(|r| r.get_mut(scol as usize))
            {
                *cell = Cell {
                    ch: ' ',
                    fg,
                    bg,
                    col: scol,
                    row,
                    url,
                    flags: CellFlags::WIDE_SPACER,
                };
            }
        }

        // Advance by the glyph width. If it reaches/overruns the final column,
        // DEFER the wrap: park the cursor on the last occupied column so the
        // wrap only happens on the next printable char (or a cursor move / CR).
        let next_col = col + w;
        if next_col >= self.cols {
            self.cursor.0 = col + w - 1;
            self.pending_wrap = true;
        } else {
            self.cursor.0 = next_col;
        }
    }

    pub(crate) fn advance_row(&mut self) {
        self.pending_wrap = false;
        if self.cursor.1 == self.scroll_bottom {
            // At the bottom margin: scroll the region instead of moving past it.
            self.scroll_up(1);
        } else if self.cursor.1 + 1 < self.rows {
            self.cursor.1 += 1;
        }
    }

    pub(crate) fn scroll_up(&mut self, n: u16) {
        let top = self.scroll_top as usize;
        let bot = (self.scroll_bottom as usize).min(self.cells.len().saturating_sub(1));
        if top > bot {
            return;
        }
        for _ in 0..n {
            if bot >= self.cells.len() {
                break;
            }
            // Drop the top region line. Only the screen-top region (top == 0)
            // of the PRIMARY screen feeds scrollback; a non-top margin or the
            // alt screen discards the line (alt screens have no scrollback).
            let removed = self.cells.remove(top);
            if top == 0 && !self.alt_screen {
                let evicted = self.scrollback.len() >= self.max_scrollback;
                if evicted {
                    self.scrollback.remove(0);
                }
                self.scrollback.push(removed);
                // If history grew without evicting a line and we're scrolled
                // back, bump the offset so the viewport stays anchored.
                if !evicted && self.scroll_offset > 0 {
                    self.scroll_offset = (self.scroll_offset + 1).min(self.max_scroll_offset());
                }
            }
            let blank = blank_row(self.cols, bot as u16);
            self.cells.insert(bot, blank);
        }
        // Re-stamp row indices so the renderer positions every cell correctly
        // after the shift.
        for (r, row) in self.cells.iter_mut().enumerate() {
            for cell in row.iter_mut() {
                cell.row = r as u16;
            }
        }
        self.lines_since_start = self.lines_since_start.saturating_add(u32::from(n));
    }

    pub(crate) fn scroll_down(&mut self, n: u16) {
        let top = self.scroll_top as usize;
        let bot = (self.scroll_bottom as usize).min(self.cells.len().saturating_sub(1));
        if top > bot {
            return;
        }
        for _ in 0..n {
            if bot >= self.cells.len() {
                break;
            }
            // Reverse scroll within the region: drop the bottom line, blank top.
            self.cells.remove(bot);
            let blank = blank_row(self.cols, top as u16);
            self.cells.insert(top, blank);
        }
        // Re-stamp row indices after shift (same reason as scroll_up).
        for (r, row) in self.cells.iter_mut().enumerate() {
            for cell in row.iter_mut() {
                cell.row = r as u16;
            }
        }
    }

    pub(crate) fn erase_display(&mut self, mode: u16) {
        let (col, row) = self.cursor;
        match mode {
            0 => {
                // Erase from cursor to end.
                if let Some(r) = self.cells.get_mut(row as usize) {
                    for c in col as usize..r.len() {
                        r[c] = Cell::blank(c as u16, row);
                    }
                }
                for r_idx in (row as usize + 1)..self.cells.len() {
                    let r = &mut self.cells[r_idx];
                    for c in 0..r.len() {
                        r[c] = Cell::blank(c as u16, r_idx as u16);
                    }
                }
            }
            1 => {
                // Erase from start to cursor.
                for r_idx in 0..row as usize {
                    let r = &mut self.cells[r_idx];
                    for c in 0..r.len() {
                        r[c] = Cell::blank(c as u16, r_idx as u16);
                    }
                }
                if let Some(r) = self.cells.get_mut(row as usize) {
                    for c in 0..=col as usize {
                        if c < r.len() {
                            r[c] = Cell::blank(c as u16, row);
                        }
                    }
                }
            }
            2 => {
                // Erase entire display.
                for r_idx in 0..self.cells.len() {
                    let r = &mut self.cells[r_idx];
                    for c in 0..r.len() {
                        r[c] = Cell::blank(c as u16, r_idx as u16);
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) fn erase_line(&mut self, mode: u16) {
        let (col, row) = self.cursor;
        if let Some(r) = self.cells.get_mut(row as usize) {
            match mode {
                0 => {
                    // Erase from cursor to end of line.
                    for c in col as usize..r.len() {
                        r[c] = Cell::blank(c as u16, row);
                    }
                }
                1 => {
                    // Erase from start to cursor.
                    for c in 0..=col as usize {
                        if c < r.len() {
                            r[c] = Cell::blank(c as u16, row);
                        }
                    }
                }
                2 => {
                    // Erase entire line.
                    for (c, cell) in r.iter_mut().enumerate() {
                        *cell = Cell::blank(c as u16, row);
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) fn enter_alt_screen(&mut self) {
        if !self.alt_screen {
            self.alt_cells = self.cells.clone();
            self.alt_cursor = self.cursor;
            self.cells = blank_grid(self.cols, self.rows);
            self.cursor = (0, 0);
            self.alt_screen = true;
            self.reset_scroll_region();
            // The alt screen has no scrollback view — snap to the live screen.
            self.scroll_offset = 0;
        }
    }

    pub(crate) fn leave_alt_screen(&mut self) {
        if self.alt_screen {
            self.cells = std::mem::take(&mut self.alt_cells);
            self.cursor = self.alt_cursor;
            self.alt_screen = false;
            self.reset_scroll_region();
            self.scroll_offset = 0;
        }
    }

    /// Resets the DECSTBM scroll region to the full screen.
    pub(crate) fn reset_scroll_region(&mut self) {
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
    }

    pub(crate) fn move_cursor(&mut self, col: u16, row: u16) {
        self.pending_wrap = false;
        self.cursor = (
            col.min(self.cols.saturating_sub(1)),
            row.min(self.rows.saturating_sub(1)),
        );
    }

    pub(crate) fn check_ps1_heuristic(&mut self, ch: char) {
        if self.osc133_seen || self.lines_since_start > 200 {
            return;
        }
        // Very coarse heuristic: detect common PS1 terminators at the end of
        // a line.  The character just placed at cursor (before advancing) is
        // the one we inspected.
        if matches!(ch, '$' | '#' | '>') {
            let row = self.cursor.1;
            self.block_markers.push(BlockMarker {
                kind: MarkerKind::PromptHeuristic,
                row,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{Cell, CellFlags};
    use crate::sgr::apply_sgr;

    fn make_grid(cols: u16, rows: u16) -> CellGrid {
        CellGrid::new(cols, rows)
    }

    #[test]
    fn grid_put_char_advances_cursor() {
        let mut grid = make_grid(80, 24);
        grid.put_char('A');
        assert_eq!(grid.cursor, (1, 0));
    }

    #[test]
    fn grid_put_char_wraps_at_column_boundary() {
        let mut grid = make_grid(4, 4);
        grid.cursor = (3, 0);
        grid.put_char('X');
        // Deferred wrap: after the last-column write the cursor stays on row 0
        // with the pending-wrap flag set; the wrap happens on the next char.
        assert_eq!(grid.cursor, (3, 0));
        assert!(grid.pending_wrap);
        grid.put_char('Y');
        assert_eq!(grid.cursor, (1, 1), "next char wraps to row 1");
    }

    #[test]
    fn deferred_wrap_does_not_advance_on_last_column() {
        let mut grid = make_grid(4, 3);
        for ch in ['a', 'b', 'c', 'd'] {
            grid.put_char(ch);
        }
        // Last-column write defers: still on row 0, no scroll.
        assert_eq!(grid.cursor.1, 0, "no premature row advance");
        assert!(grid.pending_wrap, "pending-wrap flag set");
        assert!(grid.scrollback.is_empty(), "no premature scroll");
        // The next printable char performs the wrap.
        grid.put_char('e');
        assert_eq!(grid.cursor, (1, 1), "wrapped to start of next row");
        assert_eq!(grid.cells[1][0].ch, 'e');
        assert!(!grid.pending_wrap);
    }

    #[test]
    fn bottom_right_write_does_not_eagerly_scroll() {
        // This is the ratatui corruption case: writing the bottom-right cell must
        // NOT scroll the grid (an eager wrap would).
        let mut grid = make_grid(4, 2);
        grid.cursor = (0, 1);
        for ch in ['w', 'x', 'y', 'z'] {
            grid.put_char(ch);
        }
        assert!(grid.pending_wrap);
        assert!(
            grid.scrollback.is_empty(),
            "bottom-right write must not scroll the grid"
        );
        assert_eq!(grid.cells[1][3].ch, 'z', "last cell written in place");
    }

    #[test]
    fn cursor_move_cancels_pending_wrap() {
        let mut grid = make_grid(4, 2);
        for ch in ['a', 'b', 'c', 'd'] {
            grid.put_char(ch);
        }
        assert!(grid.pending_wrap);
        grid.move_cursor(0, 0); // reposition cancels the deferred wrap
        assert!(!grid.pending_wrap);
        grid.put_char('X');
        assert_eq!(grid.cells[0][0].ch, 'X', "overwrites, no wrap");
        assert_eq!(grid.cursor, (1, 0));
    }

    #[test]
    fn grid_scroll_up_pushes_to_scrollback() {
        let mut grid = make_grid(4, 2);
        grid.cells[0][0] = Cell {
            ch: 'A',
            ..Cell::blank(0, 0)
        };
        grid.scroll_up(1);
        assert_eq!(grid.scrollback.len(), 1);
        assert_eq!(grid.scrollback[0][0].ch, 'A');
    }

    #[test]
    fn grid_scrollback_respects_max() {
        let mut grid = make_grid(4, 2);
        grid.max_scrollback = 2;
        for _ in 0..5 {
            grid.scroll_up(1);
        }
        assert!(grid.scrollback.len() <= 2);
    }

    #[test]
    fn grid_erase_line_mode2_blanks_row() {
        let mut grid = make_grid(4, 4);
        grid.cells[0][0] = Cell {
            ch: 'A',
            ..Cell::blank(0, 0)
        };
        grid.cursor = (0, 0);
        grid.erase_line(2);
        assert_eq!(grid.cells[0][0].ch, ' ');
    }

    #[test]
    fn grid_erase_display_mode2_blanks_all() {
        let mut grid = make_grid(4, 4);
        grid.cells[2][2] = Cell {
            ch: 'Z',
            ..Cell::blank(2, 2)
        };
        grid.cursor = (0, 0);
        grid.erase_display(2);
        assert_eq!(grid.cells[2][2].ch, ' ');
    }

    #[test]
    fn grid_alt_screen_saves_and_restores() {
        let mut grid = make_grid(4, 4);
        grid.cells[0][0].ch = 'M';
        grid.enter_alt_screen();
        assert!(grid.alt_screen);
        assert_eq!(grid.cells[0][0].ch, ' '); // alt screen is blank
        grid.leave_alt_screen();
        assert!(!grid.alt_screen);
        assert_eq!(grid.cells[0][0].ch, 'M');
    }

    #[test]
    fn scroll_up_restamps_row_indices() {
        let mut grid = make_grid(4, 3);
        // Fill each row with a character so we can distinguish them.
        for r in 0..3usize {
            for c in 0..4usize {
                grid.cells[r][c].row = r as u16;
            }
        }
        grid.scroll_up(1);
        // After scrolling up by 1, visual row 0 holds what was row 1.
        // All cells in cells[0] must report row=0, not the stale row=1.
        for cell in &grid.cells[0] {
            assert_eq!(cell.row, 0, "scroll_up must re-stamp row indices");
        }
        for cell in &grid.cells[1] {
            assert_eq!(cell.row, 1);
        }
        for cell in &grid.cells[2] {
            assert_eq!(cell.row, 2);
        }
    }

    #[test]
    fn scroll_down_restamps_row_indices() {
        let mut grid = make_grid(4, 3);
        for r in 0..3usize {
            for c in 0..4usize {
                grid.cells[r][c].row = r as u16;
            }
        }
        grid.scroll_down(1);
        for (r, row) in grid.cells.iter().enumerate() {
            for cell in row {
                assert_eq!(cell.row, r as u16, "scroll_down must re-stamp row indices");
            }
        }
    }

    #[test]
    fn scroll_up_n_lines_increments_lines_since_start_correctly() {
        let mut grid = make_grid(4, 3);
        grid.scroll_up(5);
        assert_eq!(grid.lines_since_start, 5, "scroll_up(n) must add n, not 1");
    }

    #[test]
    fn wide_char_sets_flags_and_spacer() {
        let mut grid = make_grid(8, 2);
        grid.put_char('世');
        assert!(grid.cells[0][0].flags.contains(CellFlags::WIDE));
        assert!(grid.cells[0][1].flags.contains(CellFlags::WIDE_SPACER));
        assert_eq!(grid.cursor.0, 2, "cursor advances by 2");
    }

    #[test]
    fn wide_char_wraps_when_it_would_overflow_last_column() {
        // 3-col grid, cursor parked at the last column: a wide glyph wraps to the
        // next row instead of splitting across the edge.
        let mut grid = make_grid(3, 3);
        grid.cursor = (2, 0);
        grid.put_char('字');
        assert_eq!(grid.cells[1][0].ch, '字', "wide glyph wrapped to next row");
        assert!(grid.cells[1][0].flags.contains(CellFlags::WIDE));
    }

    #[test]
    fn sgr_attributes_carry_onto_cells() {
        let mut grid = make_grid(8, 2);
        // bold; dim; italic; underline; strikethrough; inverse via CSI ... m,
        // then a char that should carry all of them.
        for code in [1u16, 2, 3, 4, 9, 7] {
            apply_sgr(&mut grid, &[code]);
        }
        grid.put_char('A');
        let f = grid.cells[0][0].flags;
        for flag in [
            CellFlags::BOLD,
            CellFlags::DIM,
            CellFlags::ITALIC,
            CellFlags::UNDERLINE,
            CellFlags::STRIKETHROUGH,
            CellFlags::INVERSE,
        ] {
            assert!(f.contains(flag), "missing flag {flag:?}");
        }
        // SGR 0 resets; the next char is plain.
        apply_sgr(&mut grid, &[0]);
        grid.put_char('B');
        assert_eq!(grid.cells[0][1].flags, CellFlags::empty());
    }

    #[test]
    fn alt_screen_does_not_feed_scrollback_and_snaps_scroll_offset() {
        let mut grid = make_grid(4, 3);
        grid.scroll_offset = 2; // pretend the primary screen was scrolled back
        grid.enter_alt_screen();
        assert_eq!(grid.scroll_offset, 0, "alt screen snaps to the live view");
        // Scrolling within the alt screen must NOT pollute the primary
        // scrollback (alt screens have no scrollback).
        grid.scroll_up(2);
        assert!(
            grid.scrollback.is_empty(),
            "alt-screen scroll feeds no scrollback"
        );
    }
}
