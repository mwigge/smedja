//! The terminal cell grid: screen buffer, scrollback, scroll region, cursor,
//! and the SGR / mouse / marker state the VT parser mutates.

use unicode_width::UnicodeWidthChar;

use crate::cell::{Cell, CellFlags};
use crate::color::{Color, DEFAULT_PALETTE};

// ── Mouse mode ────────────────────────────────────────────────────────────────

/// Which mouse-reporting protocol the application has enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    None,
    /// `?1000h` — click events only (press + release).
    X10,
    /// `?1002h` — button events (click + drag while button held).
    ButtonEvent,
    /// `?1003h` — any motion (click + all mouse movement).
    AnyEvent,
}

// ── SGR state ─────────────────────────────────────────────────────────────────

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
    fn cell_flags(&self) -> CellFlags {
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
}

impl SgrState {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

// ── Block markers (OSC 133 + heuristic) ───────────────────────────────────────

/// The kind of a block marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerKind {
    /// OSC 133 A — prompt start.
    PromptStart,
    /// OSC 133 B — command start.
    CommandStart,
    /// OSC 133 C — command executed.
    CommandExecuted,
    /// OSC 133 D — command done.  The payload may contain the exit code.
    CommandDone { exit_code: Option<i32> },
    /// Heuristic prompt detection (PS1 pattern match).
    PromptHeuristic,
    /// OSC 7 — current working directory notification.
    Osc7Cwd { path: String },
}

/// Marks a row as a shell integration boundary.
#[derive(Debug, Clone)]
pub struct BlockMarker {
    /// What kind of boundary this is.
    pub kind: MarkerKind,
    /// The terminal row where the marker was emitted.
    pub row: u16,
}

// ── Desktop notifications (OSC 9 / OSC 777) ──────────────────────────────────

/// Desktop notification from OSC 9 or OSC 777.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub title: String,
    pub body: String,
}

/// Parse OSC 9 payload: `OSC 9 ; <message> ST`
///
/// The payload is the raw message string; returns a notification with a
/// fixed title of `"smedja"` and the payload as the body.
#[must_use]
pub fn parse_osc9(payload: &str) -> Option<Notification> {
    Some(Notification {
        title: "smedja".into(),
        body: payload.to_owned(),
    })
}

/// Parse OSC 777 payload: `OSC 777 ; notify ; <title> ; <body> ST`
///
/// Expects the keyword `notify` as the first segment, then title and body.
/// Returns `None` for any other format.
#[must_use]
pub fn parse_osc777(payload: &str) -> Option<Notification> {
    let parts: Vec<&str> = payload.splitn(3, ';').collect();
    if parts.first().copied() == Some("notify") && parts.len() == 3 {
        Some(Notification {
            title: parts[1].trim().to_owned(),
            body: parts[2].trim().to_owned(),
        })
    } else {
        None
    }
}

/// Parse an OSC 7 URI (`file://hostname/path` or `file:///path`) into a path string.
///
/// Returns `None` if the URI does not start with `file://`.
#[must_use]
pub fn parse_osc7_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///path` → hostname is empty, rest starts with `/path`
    // `file://host/path` → skip to the first `/`
    let path = if rest.starts_with('/') {
        rest.to_owned()
    } else {
        rest.find('/').map(|i| rest[i..].to_owned())?
    };
    Some(path)
}

// ── CellGrid ──────────────────────────────────────────────────────────────────

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
    alt_cells: Vec<Vec<Cell>>,
    /// Alternate screen cursor.
    alt_cursor: (u16, u16),
    /// Shell integration markers.
    pub block_markers: Vec<BlockMarker>,
    /// Desktop notifications received via OSC 9 or OSC 777.
    pub notifications: Vec<Notification>,
    /// Current scroll offset: 0 = live view, positive = scrolled back.
    pub scroll_offset: i32,
    /// Current SGR state.
    pub(crate) sgr: SgrState,
    /// ANSI palette (forged_terminal defaults).
    palette: [[f32; 4]; 16],
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

    fn resize_cells(cells: &mut Vec<Vec<Cell>>, cols: u16, rows: u16) {
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

    // ── internal screen mutations ─────────────────────────────────────────────

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

// ── Grid utilities ────────────────────────────────────────────────────────────

pub(crate) fn blank_row(cols: u16, row: u16) -> Vec<Cell> {
    (0..cols).map(|c| Cell::blank(c, row)).collect()
}

fn blank_grid(cols: u16, rows: u16) -> Vec<Vec<Cell>> {
    (0..rows).map(|r| blank_row(cols, r)).collect()
}
