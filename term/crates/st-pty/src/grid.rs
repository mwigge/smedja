//! The terminal cell grid: active screen, scrollback, scroll region, and the
//! viewport window used for scroll-back rendering.

use crate::cell::Cell;
use crate::color::DEFAULT_PALETTE;
use crate::markers::BlockMarker;
use crate::mouse::MouseMode;
use crate::notification::Notification;
use crate::sgr::SgrState;

/// The full terminal cell grid, including scrollback.
#[allow(clippy::struct_excessive_bools)]
pub struct CellGrid {
    /// Number of columns in the active area.
    pub cols: u16,
    /// Number of rows in the active area.
    pub rows: u16,
    /// Active screen: `cells[row][col]`.
    pub cells: Vec<Vec<Cell>>,
    /// Cursor position as `(col, row)`.
    pub cursor: (u16, u16),
    /// Deferred-wrap (last-column) flag. Set after a glyph is written to the
    /// final column; the actual line wrap is postponed until the *next*
    /// printable character. This matches xterm/DEC behaviour and is essential
    /// for full-screen apps (ratatui): an eager wrap on the bottom-right cell
    /// would scroll the whole grid and desync the client's diff.
    pub(crate) pending_wrap: bool,
    /// Top margin of the scroll region (0-based, inclusive). DECSTBM.
    pub(crate) scroll_top: u16,
    /// Bottom margin of the scroll region (0-based, inclusive). DECSTBM.
    pub(crate) scroll_bottom: u16,
    /// Scrollback lines (oldest first).
    pub scrollback: Vec<Vec<Cell>>,
    /// Maximum number of scrollback lines.
    pub max_scrollback: usize,
    /// True while the alternate screen is active.
    pub alt_screen: bool,
    /// Alternate screen cells (saved during main→alt transition).
    pub(crate) alt_cells: Vec<Vec<Cell>>,
    /// Alternate screen cursor.
    pub(crate) alt_cursor: (u16, u16),
    /// Shell integration markers.
    pub block_markers: Vec<BlockMarker>,
    /// Desktop notifications received via OSC 9 or OSC 777.
    pub notifications: Vec<Notification>,
    /// Current scroll offset: 0 = live view, positive = scrolled back.
    pub scroll_offset: i32,
    /// Current SGR state.
    pub(crate) sgr: SgrState,
    /// ANSI palette (forged_terminal defaults).
    pub(crate) palette: [[f32; 4]; 16],
    /// Whether OSC 133 has been observed yet.
    pub(crate) osc133_seen: bool,
    /// Monotonic line count since start (for heuristic timing).
    pub(crate) lines_since_start: u32,
    /// Window title set via OSC 0 or OSC 2.
    pub title: Option<String>,
    /// Saved cursor position (ESC 7 / ESC 8 DEC save/restore).
    pub cursor_saved: Option<(u16, u16)>,
    /// Whether the cursor is visible (`true` by default; `?25l` hides it).
    pub cursor_visible: bool,
    /// Active mouse-reporting mode (set by the application via DEC private modes).
    pub mouse_mode: MouseMode,
    /// Whether SGR extended mouse coordinates are active (`?1006h`).
    pub mouse_sgr: bool,
    /// Whether bracketed paste mode is active (`?2004h`).
    pub bracketed_paste: bool,
    /// Whether focus-in/out events are enabled (`?1004h`).
    pub focus_events: bool,
    /// Whether synchronized output mode is active (`?2026h`).
    pub synchronized_output: bool,
    /// Decoded clipboard text to be written, drained by the reader thread after each batch.
    pub pending_clipboard_write: Option<String>,
    /// Kitty keyboard protocol enhancement flags, managed as a stack (one entry
    /// per `CSI > flags u` push). The top is the active flag set; an empty stack
    /// means legacy encoding (no enhancement). See [`CellGrid::kbd_flags`].
    pub kbd_flags_stack: Vec<u8>,
    /// Bytes queued to write back to the PTY master — e.g. the kitty keyboard
    /// protocol query response (`CSI ? flags u`). Drained by the host each frame.
    pub pending_responses: Vec<u8>,
}

impl CellGrid {
    /// Creates a blank grid of `cols×rows` cells.
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        let cells = blank_grid(cols, rows);
        Self {
            cols,
            rows,
            cells,
            cursor: (0, 0),
            pending_wrap: false,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            scrollback: Vec::new(),
            max_scrollback: 10_000,
            alt_screen: false,
            alt_cells: Vec::new(),
            alt_cursor: (0, 0),
            block_markers: Vec::new(),
            notifications: Vec::new(),
            scroll_offset: 0,
            sgr: SgrState::default(),
            palette: DEFAULT_PALETTE,
            osc133_seen: false,
            lines_since_start: 0,
            title: None,
            cursor_saved: None,
            cursor_visible: true,
            mouse_mode: MouseMode::None,
            mouse_sgr: false,
            bracketed_paste: false,
            focus_events: false,
            synchronized_output: false,
            pending_clipboard_write: None,
            kbd_flags_stack: Vec::new(),
            pending_responses: Vec::new(),
        }
    }

    /// Returns the active kitty keyboard protocol flags, or `0` when the stack
    /// is empty (legacy encoding). Bit 1 = disambiguate escape codes, bit 2 =
    /// report event types, bit 4 = report alternate keys, bit 8 = report all
    /// keys as escape codes, bit 16 = report associated text.
    #[must_use]
    pub fn kbd_flags(&self) -> u8 {
        self.kbd_flags_stack.last().copied().unwrap_or(0)
    }

    /// Resizes the grid.  Content is preserved where possible.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        Self::resize_cells(&mut self.cells, cols, rows);
        // Keep alt_cells in sync so that leaving alt screen after a resize
        // restores a correctly-sized grid instead of causing dimension mismatches.
        if self.alt_screen {
            Self::resize_cells(&mut self.alt_cells, cols, rows);
        }
        // Clamp cursor.
        self.cursor.0 = self.cursor.0.min(cols.saturating_sub(1));
        self.cursor.1 = self.cursor.1.min(rows.saturating_sub(1));
        // Resetting the scroll region to the full screen on resize matches xterm.
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
        self.pending_wrap = false;
    }

    pub(crate) fn resize_cells(cells: &mut Vec<Vec<Cell>>, cols: u16, rows: u16) {
        cells.resize_with(rows as usize, Vec::new);
        for (r, row) in cells.iter_mut().enumerate() {
            row.resize_with(cols as usize, || Cell::blank(0, r as u16));
            for (c, cell) in row.iter_mut().enumerate() {
                cell.col = c as u16;
                cell.row = r as u16;
            }
        }
    }

    /// Returns the `rows` rows visible at the given scroll-back `offset`.
    ///
    /// The full terminal buffer is conceptually `scrollback ++ cells`. `offset`
    /// 0 is the live screen (the last `rows` rows = `cells`); a positive offset
    /// shifts the viewport up by that many lines into history, so the window can
    /// straddle the scrollback/live boundary (the top from scrollback, the
    /// bottom from the live screen). `offset` is clamped to the available
    /// history. Fewer than `rows` rows are returned only when the live screen
    /// itself is shorter than `rows`.
    #[must_use]
    pub fn visible_rows(&self, offset: i32) -> Vec<&Vec<Cell>> {
        let rows = self.rows as usize;
        if offset <= 0 {
            return self.cells.iter().collect();
        }
        let sb = self.scrollback.len();
        let skip = (offset as usize).min(sb);
        // Window over the combined buffer: [sb - skip .. sb - skip + rows].
        let start = sb - skip;
        let mut out: Vec<&Vec<Cell>> = Vec::with_capacity(rows);
        for i in start..start + rows {
            if i < sb {
                out.push(&self.scrollback[i]);
            } else if let Some(row) = self.cells.get(i - sb) {
                out.push(row);
            }
        }
        out
    }

    /// Maximum scroll-back offset (number of history lines available).
    #[must_use]
    pub fn max_scroll_offset(&self) -> i32 {
        i32::try_from(self.scrollback.len()).unwrap_or(i32::MAX)
    }

    /// Adjusts `scroll_offset` by `delta` lines (positive = into history),
    /// clamped to `[0, max_scroll_offset()]`. Returns `true` if it changed.
    pub fn scroll_by(&mut self, delta: i32) -> bool {
        let max = self.max_scroll_offset();
        let next = (self.scroll_offset + delta).clamp(0, max);
        if next == self.scroll_offset {
            false
        } else {
            self.scroll_offset = next;
            true
        }
    }
}

// ── Grid utilities ────────────────────────────────────────────────────────────

pub(crate) fn blank_row(cols: u16, row: u16) -> Vec<Cell> {
    (0..cols).map(|c| Cell::blank(c, row)).collect()
}

pub(crate) fn blank_grid(cols: u16, rows: u16) -> Vec<Vec<Cell>> {
    (0..rows).map(|r| blank_row(cols, r)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_grid(cols: u16, rows: u16) -> CellGrid {
        CellGrid::new(cols, rows)
    }

    /// Stamps `cells[row][0]` with `ch` so a row is identifiable in assertions.
    fn mark(grid: &mut CellGrid, row: usize, ch: char) {
        grid.cells[row][0] = Cell {
            ch,
            ..Cell::blank(0, row as u16)
        };
    }

    #[test]
    fn visible_rows_offset_zero_is_live_screen() {
        let mut grid = make_grid(4, 2);
        mark(&mut grid, 0, 'L');
        let view = grid.visible_rows(0);
        assert_eq!(view.len(), 2);
        assert_eq!(view[0][0].ch, 'L', "offset 0 shows the live screen");
    }

    #[test]
    fn visible_rows_straddles_scrollback_and_live() {
        // 2-row screen. Push rows 'A' then 'B' into scrollback; live screen
        // then holds the post-scroll blanks with 'C' marked on the top row.
        let mut grid = make_grid(4, 2);
        mark(&mut grid, 0, 'A');
        grid.scroll_up(1); // 'A' -> scrollback[0]
        mark(&mut grid, 0, 'B');
        grid.scroll_up(1); // 'B' -> scrollback[1]
        mark(&mut grid, 0, 'C'); // live top row
        assert_eq!(grid.scrollback.len(), 2);
        // Offset 1: window shows [scrollback[1]='B', live[0]='C'].
        let view = grid.visible_rows(1);
        assert_eq!(view.len(), 2);
        assert_eq!(view[0][0].ch, 'B', "top of view from scrollback");
        assert_eq!(view[1][0].ch, 'C', "bottom of view from live screen");
    }

    #[test]
    fn scroll_by_clamps_to_history_bounds() {
        let mut grid = make_grid(4, 2);
        for _ in 0..3 {
            grid.scroll_up(1);
        }
        assert_eq!(grid.max_scroll_offset(), 3);
        assert!(grid.scroll_by(100)); // clamps to 3
        assert_eq!(grid.scroll_offset, 3);
        assert!(!grid.scroll_by(10), "already at max, no change");
        assert!(grid.scroll_by(-100)); // clamps to 0
        assert_eq!(grid.scroll_offset, 0);
        assert!(!grid.scroll_by(-1), "already at live, no change");
    }

    #[test]
    fn scroll_up_anchors_viewport_when_history_grows() {
        let mut grid = make_grid(4, 2);
        grid.scroll_up(1); // history = 1
        grid.scroll_by(1); // viewing the single history line (offset 1)
        assert_eq!(grid.scroll_offset, 1);
        // New output scrolls another line into history; offset bumps to keep the
        // same content in view.
        grid.scroll_up(1);
        assert_eq!(grid.scroll_offset, 2, "offset tracks history growth");
    }

    #[test]
    fn grid_resize_clamps_cursor() {
        let mut grid = make_grid(80, 24);
        grid.cursor = (79, 23);
        grid.resize(40, 10);
        assert!(grid.cursor.0 < 40);
        assert!(grid.cursor.1 < 10);
    }

    #[test]
    fn resize_growing_preserves_content() {
        let mut grid = make_grid(4, 4);
        grid.cells[0][0].ch = 'X';
        grid.resize(8, 8);
        assert_eq!(grid.cols, 8);
        assert_eq!(grid.rows, 8);
        assert_eq!(grid.cells[0][0].ch, 'X');
        assert_eq!(grid.cells.len(), 8);
        assert_eq!(grid.cells[0].len(), 8);
    }

    #[test]
    fn resize_shrinking_clips_content() {
        let mut grid = make_grid(8, 8);
        grid.cells[0][0].ch = 'A';
        grid.resize(4, 4);
        assert_eq!(grid.cols, 4);
        assert_eq!(grid.rows, 4);
        assert_eq!(grid.cells[0][0].ch, 'A');
        assert_eq!(grid.cells.len(), 4);
        assert_eq!(grid.cells[0].len(), 4);
    }

    #[test]
    fn resize_clamps_cursor_to_new_bounds() {
        let mut grid = make_grid(10, 10);
        grid.cursor = (9, 9);
        grid.resize(4, 4);
        assert!(grid.cursor.0 < 4);
        assert!(grid.cursor.1 < 4);
    }

    #[test]
    fn resize_also_resizes_alt_cells_when_alt_screen_active() {
        let mut grid = make_grid(80, 24);
        grid.enter_alt_screen();
        assert!(grid.alt_screen);
        // Resize while alt screen is active.
        grid.resize(40, 12);
        // Leave alt screen — must not panic and must have correct dimensions.
        grid.leave_alt_screen();
        assert_eq!(
            grid.cells.len(),
            12,
            "restored cells must have new row count"
        );
        assert_eq!(
            grid.cells[0].len(),
            40,
            "restored cells must have new col count"
        );
    }

    #[test]
    fn scroll_region_resets_to_full_screen_on_resize() {
        let mut grid = make_grid(4, 4);
        grid.scroll_top = 1;
        grid.scroll_bottom = 2;
        grid.resize(4, 4);
        assert_eq!(grid.scroll_top, 0);
        assert_eq!(grid.scroll_bottom, 3);
    }
}
