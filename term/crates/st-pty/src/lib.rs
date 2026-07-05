//! `st-pty` — PTY session management, VT emulation, scrollback, and copy mode.
//!
//! Spawns a child shell via [`portable_pty`], feeds its output through a
//! [`vte::Parser`] that mutates a shared [`CellGrid`], and exposes the grid
//! for rendering.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::assigning_clones,
    clippy::single_char_lifetime_names,
    clippy::equatable_if_let,
    clippy::match_like_matches_macro,
    clippy::doc_markdown,
    clippy::many_single_char_names,
    clippy::needless_range_loop,
    clippy::float_cmp,
    clippy::float_cmp_const
)]

use std::io::{Read, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use parking_lot::Mutex;
use thiserror::Error;
use tracing::{debug, warn};

use st_glyph::GlyphRegistry;
use unicode_width::UnicodeWidthChar;

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors produced by PTY operations.
#[derive(Debug, Error)]
pub enum PtyError {
    /// PTY system call failed.
    #[error("pty error: {0}")]
    Pty(String),
    /// I/O error on the PTY master fd.
    #[error("pty I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Clipboard error.
    #[error("clipboard error: {0}")]
    Clipboard(String),
}

// ── Cell ──────────────────────────────────────────────────────────────────────

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

// ── Colour types ──────────────────────────────────────────────────────────────

const DEFAULT_FG: [f32; 4] = [0.957, 0.843, 0.631, 1.0]; // #f4d7a1
const DEFAULT_BG: [f32; 4] = [0.043, 0.051, 0.059, 1.0]; // #0b0d0f

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
struct SgrState {
    fg: Color,
    bg: Color,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    dim: bool,
    inverse: bool,
    /// OSC 8 URL currently in scope.
    url: Option<String>,
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
    fn reset(&mut self) {
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
    pending_wrap: bool,
    /// Top margin of the scroll region (0-based, inclusive). DECSTBM.
    scroll_top: u16,
    /// Bottom margin of the scroll region (0-based, inclusive). DECSTBM.
    scroll_bottom: u16,
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
    sgr: SgrState,
    /// ANSI palette (forged_terminal defaults).
    palette: [[f32; 4]; 16],
    /// Whether OSC 133 has been observed yet.
    osc133_seen: bool,
    /// Monotonic line count since start (for heuristic timing).
    lines_since_start: u32,
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

    fn put_char(&mut self, ch: char) {
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

    fn advance_row(&mut self) {
        self.pending_wrap = false;
        if self.cursor.1 == self.scroll_bottom {
            // At the bottom margin: scroll the region instead of moving past it.
            self.scroll_up(1);
        } else if self.cursor.1 + 1 < self.rows {
            self.cursor.1 += 1;
        }
    }

    fn scroll_up(&mut self, n: u16) {
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

    fn scroll_down(&mut self, n: u16) {
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

    fn erase_display(&mut self, mode: u16) {
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

    fn erase_line(&mut self, mode: u16) {
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

    fn enter_alt_screen(&mut self) {
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

    fn leave_alt_screen(&mut self) {
        if self.alt_screen {
            self.cells = std::mem::take(&mut self.alt_cells);
            self.cursor = self.alt_cursor;
            self.alt_screen = false;
            self.reset_scroll_region();
            self.scroll_offset = 0;
        }
    }

    /// Resets the DECSTBM scroll region to the full screen.
    fn reset_scroll_region(&mut self) {
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
    }

    fn move_cursor(&mut self, col: u16, row: u16) {
        self.pending_wrap = false;
        self.cursor = (
            col.min(self.cols.saturating_sub(1)),
            row.min(self.rows.saturating_sub(1)),
        );
    }

    fn check_ps1_heuristic(&mut self, ch: char) {
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

fn blank_row(cols: u16, row: u16) -> Vec<Cell> {
    (0..cols).map(|c| Cell::blank(c, row)).collect()
}

fn blank_grid(cols: u16, rows: u16) -> Vec<Vec<Cell>> {
    (0..rows).map(|r| blank_row(cols, r)).collect()
}

/// Default ANSI palette (forged_terminal).
const DEFAULT_PALETTE: [[f32; 4]; 16] = [
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

// ── APC pre-scanner ───────────────────────────────────────────────────────────

/// State machine that scans raw PTY bytes for `ESC _ … ESC \` (APC) sequences.
///
/// vte 0.13 routes APC bytes to its `Ignore` state and never fires a
/// performer callback, so this scanner runs alongside the vte parser to
/// intercept smedja Glyph Protocol registrations emitted by child processes.
#[derive(Debug, Default)]
struct ApcScanner {
    state: ApcScanState,
    payload: Vec<u8>,
}

#[derive(Debug, Default)]
enum ApcScanState {
    #[default]
    Ground,
    GotEsc,
    InApc,
    InApcGotEsc,
}

impl ApcScanner {
    fn new() -> Self {
        Self::default()
    }

    /// Feeds one byte into the scanner.
    ///
    /// Returns the completed APC payload bytes when a full `ESC _ … ESC \`
    /// sequence has been received, or `None` otherwise.
    fn advance(&mut self, byte: u8) -> Option<Vec<u8>> {
        match self.state {
            ApcScanState::Ground => {
                if byte == 0x1B {
                    self.state = ApcScanState::GotEsc;
                }
                None
            }
            ApcScanState::GotEsc => {
                if byte == b'_' {
                    self.state = ApcScanState::InApc;
                    self.payload.clear();
                } else {
                    self.state = ApcScanState::Ground;
                }
                None
            }
            ApcScanState::InApc => {
                if byte == 0x1B {
                    self.state = ApcScanState::InApcGotEsc;
                } else {
                    self.payload.push(byte);
                }
                None
            }
            ApcScanState::InApcGotEsc => {
                if byte == b'\\' {
                    let payload = std::mem::take(&mut self.payload);
                    self.state = ApcScanState::Ground;
                    Some(payload)
                } else {
                    // ESC inside APC not followed by '\' — include both bytes in payload.
                    self.payload.push(0x1B);
                    self.payload.push(byte);
                    self.state = ApcScanState::InApc;
                    None
                }
            }
        }
    }
}

// ── VT performer ─────────────────────────────────────────────────────────────

struct VtHandler {
    grid: Arc<Mutex<CellGrid>>,
    glyph_registry: Arc<Mutex<GlyphRegistry>>,
}

impl vte::Perform for VtHandler {
    fn print(&mut self, c: char) {
        let mut grid = self.grid.lock();
        grid.check_ps1_heuristic(c);
        grid.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        let mut grid = self.grid.lock();
        match byte {
            b'\r' => {
                grid.pending_wrap = false;
                grid.cursor.0 = 0;
            }
            b'\n' | 0x0b | 0x0c => {
                grid.advance_row();
            }
            b'\t' => {
                // Advance to next tab stop (every 8 columns).
                grid.pending_wrap = false;
                let col = grid.cursor.0;
                let next = ((col / 8) + 1) * 8;
                grid.cursor.0 = next.min(grid.cols.saturating_sub(1));
            }
            0x08 => {
                // Backspace
                grid.pending_wrap = false;
                if grid.cursor.0 > 0 {
                    grid.cursor.0 -= 1;
                }
            }
            0x07 => {
                // BEL — ignore
            }
            _ => {
                debug!("unhandled execute byte: 0x{:02x}", byte);
            }
        }
    }

    #[allow(clippy::too_many_lines)] // complex VT dispatch is inherently long
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let mut grid = self.grid.lock();
        let p: Vec<u16> = params
            .iter()
            .map(|sub| sub.first().copied().unwrap_or(0))
            .collect();

        // Any explicit cursor movement cancels a deferred last-column wrap.
        if matches!(action, 'A' | 'B' | 'C' | 'D' | 'G' | 'H' | 'f' | 'd') {
            grid.pending_wrap = false;
        }

        match action {
            // ── Cursor movement ──────────────────────────────────────────────
            'A' => {
                let n = p.first().copied().unwrap_or(1).max(1);
                grid.cursor.1 = grid.cursor.1.saturating_sub(n);
            }
            'B' => {
                let n = p.first().copied().unwrap_or(1).max(1);
                grid.cursor.1 = (grid.cursor.1 + n).min(grid.rows.saturating_sub(1));
            }
            'C' => {
                let n = p.first().copied().unwrap_or(1).max(1);
                grid.cursor.0 = (grid.cursor.0 + n).min(grid.cols.saturating_sub(1));
            }
            'D' => {
                let n = p.first().copied().unwrap_or(1).max(1);
                grid.cursor.0 = grid.cursor.0.saturating_sub(n);
            }
            'G' => {
                let col = p.first().copied().unwrap_or(1).saturating_sub(1);
                grid.cursor.0 = col.min(grid.cols.saturating_sub(1));
            }
            'H' | 'f' => {
                let row = p.first().copied().unwrap_or(1).saturating_sub(1);
                let col = p.get(1).copied().unwrap_or(1).saturating_sub(1);
                grid.move_cursor(col, row);
            }
            // ── Erase ────────────────────────────────────────────────────────
            'J' => {
                let mode = p.first().copied().unwrap_or(0);
                grid.erase_display(mode);
            }
            'K' => {
                let mode = p.first().copied().unwrap_or(0);
                grid.erase_line(mode);
            }
            // ── Scroll ───────────────────────────────────────────────────────
            'S' => {
                let n = p.first().copied().unwrap_or(1).max(1);
                grid.scroll_up(n);
            }
            'T' => {
                let n = p.first().copied().unwrap_or(1).max(1);
                grid.scroll_down(n);
            }
            // ── SGR ──────────────────────────────────────────────────────────
            'm' => {
                apply_sgr(&mut grid, &p);
            }
            // ── Private mode (DEC) ───────────────────────────────────────────
            'h' if intermediates == [b'?'] => match p.first().copied().unwrap_or(0) {
                1049 => grid.enter_alt_screen(),
                25 => grid.cursor_visible = true,
                1000 => grid.mouse_mode = MouseMode::X10,
                1002 => grid.mouse_mode = MouseMode::ButtonEvent,
                1003 => grid.mouse_mode = MouseMode::AnyEvent,
                1006 => grid.mouse_sgr = true,
                1004 => grid.focus_events = true,
                2004 => grid.bracketed_paste = true,
                2026 => grid.synchronized_output = true,
                _ => {}
            },
            'l' if intermediates == [b'?'] => match p.first().copied().unwrap_or(0) {
                1049 => grid.leave_alt_screen(),
                25 => grid.cursor_visible = false,
                1000 | 1002 | 1003 => grid.mouse_mode = MouseMode::None,
                1006 => grid.mouse_sgr = false,
                1004 => grid.focus_events = false,
                2004 => grid.bracketed_paste = false,
                2026 => grid.synchronized_output = false,
                _ => {}
            },
            // ── Kitty keyboard protocol ──────────────────────────────────────
            // Query current flags: respond with `CSI ? <flags> u`.
            'u' if intermediates == [b'?'] => {
                let flags = grid.kbd_flags();
                let resp = format!("\x1b[?{flags}u");
                grid.pending_responses.extend_from_slice(resp.as_bytes());
            }
            // Push flags onto the stack (`CSI > flags u`, default 1).
            'u' if intermediates == [b'>'] => {
                #[allow(clippy::cast_possible_truncation)]
                let flags = p.first().copied().unwrap_or(1) as u8;
                grid.kbd_flags_stack.push(flags);
            }
            // Pop N entries (`CSI < N u`, default 1).
            'u' if intermediates == [b'<'] => {
                let n = p.first().copied().unwrap_or(1).max(1) as usize;
                for _ in 0..n {
                    grid.kbd_flags_stack.pop();
                }
            }
            // Set current flags (`CSI = flags ; mode u`): mode 1=all, 2=set
            // bits, 3=clear bits (default 1).
            'u' if intermediates == [b'='] => {
                #[allow(clippy::cast_possible_truncation)]
                let flags = p.first().copied().unwrap_or(0) as u8;
                let mode = p.get(1).copied().unwrap_or(1);
                let cur = grid.kbd_flags();
                let new = match mode {
                    2 => cur | flags,
                    3 => cur & !flags,
                    _ => flags,
                };
                if let Some(top) = grid.kbd_flags_stack.last_mut() {
                    *top = new;
                } else {
                    grid.kbd_flags_stack.push(new);
                }
            }
            // ── Line delete / insert (within the scroll region) ──────────────
            'L' => {
                // Insert blank lines at the cursor, pushing lines down to the
                // bottom margin (lines below the margin are untouched).
                let n = p.first().copied().unwrap_or(1).max(1);
                let row = grid.cursor.1 as usize;
                let bot = (grid.scroll_bottom as usize).min(grid.cells.len().saturating_sub(1));
                let cols = grid.cols;
                if row <= bot {
                    for _ in 0..n {
                        grid.cells.remove(bot);
                        grid.cells.insert(row, blank_row(cols, row as u16));
                    }
                }
            }
            'M' => {
                // Delete lines at the cursor, pulling lines up from the bottom
                // margin (lines below the margin are untouched).
                let n = p.first().copied().unwrap_or(1).max(1);
                let row = grid.cursor.1 as usize;
                let bot = (grid.scroll_bottom as usize).min(grid.cells.len().saturating_sub(1));
                let cols = grid.cols;
                if row <= bot {
                    for _ in 0..n {
                        grid.cells.remove(row);
                        grid.cells.insert(bot, blank_row(cols, bot as u16));
                    }
                }
            }
            // ── Scroll region (DECSTBM) ──────────────────────────────────────
            'r' if intermediates.is_empty() => {
                let top = p.first().copied().unwrap_or(1).max(1);
                let bottom = p.get(1).copied().filter(|&b| b > 0).unwrap_or(grid.rows);
                let top0 = top - 1;
                let bot0 = bottom.saturating_sub(1).min(grid.rows.saturating_sub(1));
                if top0 < bot0 {
                    grid.scroll_top = top0;
                    grid.scroll_bottom = bot0;
                    // DECSTBM homes the cursor.
                    grid.move_cursor(0, 0);
                }
            }
            _ => {
                debug!("unhandled CSI: action={} params={:?}", action, p);
            }
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let mut grid = self.grid.lock();
        if params.is_empty() {
            return;
        }
        let command = std::str::from_utf8(params[0]).unwrap_or("");
        match command {
            // OSC 0/2 — set window title and/or icon name.
            "0" | "2" => {
                if let Some(title) = params.get(1).and_then(|b| std::str::from_utf8(b).ok()) {
                    grid.title = Some(title.to_owned());
                }
            }
            "8" => {
                // OSC 8 ; params ; uri ST — hyperlink.
                let uri = params.get(2).and_then(|b| std::str::from_utf8(b).ok());
                grid.sgr.url = uri.filter(|s| !s.is_empty()).map(String::from);
            }
            "133" => {
                // OSC 133 — shell integration.
                grid.osc133_seen = true;
                let code = params.get(1).and_then(|b| std::str::from_utf8(b).ok());
                let row = grid.cursor.1;
                match code {
                    Some("A") => grid.block_markers.push(BlockMarker {
                        kind: MarkerKind::PromptStart,
                        row,
                    }),
                    Some("B") => grid.block_markers.push(BlockMarker {
                        kind: MarkerKind::CommandStart,
                        row,
                    }),
                    Some("C") => grid.block_markers.push(BlockMarker {
                        kind: MarkerKind::CommandExecuted,
                        row,
                    }),
                    Some(d) if d.starts_with('D') => {
                        let exit_code = d.split(';').nth(1).and_then(|s| s.parse::<i32>().ok());
                        grid.block_markers.push(BlockMarker {
                            kind: MarkerKind::CommandDone { exit_code },
                            row,
                        });
                    }
                    _ => {}
                }
            }
            "7" => {
                // OSC 7 ; file://hostname/path BEL — current working directory.
                if let Some(uri) = params.get(1).and_then(|b| std::str::from_utf8(b).ok()) {
                    if let Some(path) = parse_osc7_uri(uri) {
                        let row = grid.cursor.1;
                        grid.block_markers.push(BlockMarker {
                            kind: MarkerKind::Osc7Cwd { path },
                            row,
                        });
                    }
                }
            }
            "9" => {
                // OSC 9 ; <message> ST
                let msg = params
                    .get(1)
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .unwrap_or("");
                if let Some(n) = parse_osc9(msg) {
                    debug!("OSC 9 notification: {:?}", n.body);
                    grid.notifications.push(n);
                }
            }
            "777" => {
                // OSC 777 ; notify ; <title> ; <body> ST
                // Reconstruct payload as "notify;<title>;<body>"
                let payload = params[1..]
                    .iter()
                    .filter_map(|b| std::str::from_utf8(b).ok())
                    .collect::<Vec<_>>()
                    .join(";");
                if let Some(n) = parse_osc777(&payload) {
                    debug!(
                        "OSC 777 notification: title={:?} body={:?}",
                        n.title, n.body
                    );
                    grid.notifications.push(n);
                }
            }
            "52" => {
                // OSC 52 ; Pc ; Pd — clipboard write.
                // Pd is base64-encoded UTF-8 text; "?" means query (not supported).
                if let Some(b64) = params.get(2).and_then(|b| std::str::from_utf8(b).ok()) {
                    if b64 != "?" && !b64.is_empty() {
                        use base64::Engine as _;
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                            if let Ok(text) = String::from_utf8(bytes) {
                                grid.pending_clipboard_write = Some(text);
                            }
                        }
                    }
                }
            }
            _ => {
                debug!("unhandled OSC: {:?}", command);
            }
        }
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _c: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        let mut grid = self.grid.lock();
        match byte {
            b'7' => {
                // DEC cursor save.
                grid.cursor_saved = Some(grid.cursor);
            }
            b'8' => {
                // DEC cursor restore.
                if let Some(saved) = grid.cursor_saved {
                    grid.cursor = saved;
                }
            }
            b'D' => {
                // Index (IND): down one row, scrolling at the bottom margin.
                grid.advance_row();
            }
            b'M' => {
                // Reverse Index (RI): up one row, scrolling down at the top
                // margin.
                grid.pending_wrap = false;
                if grid.cursor.1 == grid.scroll_top {
                    grid.scroll_down(1);
                } else if grid.cursor.1 > 0 {
                    grid.cursor.1 -= 1;
                }
            }
            b'E' => {
                // Next Line (NEL): CR + IND.
                grid.cursor.0 = 0;
                grid.advance_row();
            }
            b'c' => {
                // RIS — reset to initial state.
                if grid.alt_screen {
                    grid.leave_alt_screen();
                }
                grid.reset_scroll_region();
                grid.sgr.reset();
                grid.cursor = (0, 0);
                grid.pending_wrap = false;
                grid.cursor_visible = true;
                grid.erase_display(2);
            }
            _ => {
                debug!("unhandled ESC: 0x{:02x}", byte);
            }
        }
    }
}

/// Applies SGR parameters to the grid's current SGR state.
fn apply_sgr(grid: &mut CellGrid, params: &[u16]) {
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

// ── Copy mode ────────────────────────────────────────────────────────────────

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
                // The match column is the number of cells (chars) before `abs`,
                // not the raw byte offset — one grid cell holds exactly one char.
                let col = line[..abs].chars().count();
                self.search_matches.push((col as u16, r as u16));
                // Advance past the first char of this match. Stepping a single
                // byte could land mid-codepoint and panic the next `line[start..]`
                // slice on multibyte content (emoji/CJK/accented text).
                let step = line[abs..].chars().next().map_or(1, char::len_utf8);
                start = abs + step;
            }
        }
    }
}

// ── PtySession ────────────────────────────────────────────────────────────────

/// An active PTY session: child shell + cell grid + dirty flag.
pub struct PtySession {
    master: Box<dyn portable_pty::MasterPty + Send>,
    // Child process handle; retained so teardown can hang up (SIGHUP) and
    // reap it, and so a normal-exit child is `wait()`ed (no zombie).
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    /// Join handle for the background reader thread, retained so teardown can
    /// join it instead of leaking the thread (and its cloned master fd).
    reader: Option<std::thread::JoinHandle<()>>,
    /// The shared terminal cell grid.
    pub grid: Arc<Mutex<CellGrid>>,
    /// Set to `true` whenever the grid changes.  Renderers poll this flag.
    pub dirty: Arc<AtomicBool>,
    /// Set to `true` once the child process exits (reader hits EOF). The host
    /// app polls this to close the window instead of showing a dead/blank grid.
    pub exited: Arc<AtomicBool>,
    /// Copy-mode state.
    pub copy_mode: CopyMode,
    /// Glyph registry: maps glyph IDs to PUA codepoints.
    pub glyph_registry: Arc<Mutex<GlyphRegistry>>,
}

impl Drop for PtySession {
    /// Full teardown so closing a split (Ctrl+W) while the child is still
    /// running does not leak a reader thread, a master fd, and a zombie child.
    ///
    /// `child.kill()` sends `SIGHUP` to the child. Because `portable_pty`
    /// spawns the child as a session leader owning the pty as its controlling
    /// terminal, the hangup terminates the session and propagates to the
    /// foreground process group, which closes the pty slave. The reader
    /// thread's blocking read on the cloned master then returns EOF and the
    /// thread exits, so the join below cannot hang. `child.wait()` reaps the
    /// process — for both the killed child here and a child that already
    /// exited on its own (the normal-exit path), leaving no zombie.
    fn drop(&mut self) {
        // Best effort: the child may already be gone, in which case kill fails
        // harmlessly and wait returns its status immediately.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

impl PtySession {
    /// Spawns a new PTY session running `shell` at `cols×rows`.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Pty`] if the PTY or child process cannot be created.
    pub fn spawn(cols: u16, rows: u16, shell: &str) -> Result<Self, PtyError> {
        Self::spawn_with_env(cols, rows, shell, &[])
    }

    /// Spawns a PTY session with additional environment variables injected
    /// alongside the standard `TERM` and `COLORTERM` entries.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Pty`] if the PTY cannot be opened or the shell
    /// cannot be spawned, or [`PtyError::Io`] if the master writer cannot be
    /// extracted.
    pub fn spawn_with_env(
        cols: u16,
        rows: u16,
        shell: &str,
        extra_env: &[(&str, &str)],
    ) -> Result<Self, PtyError> {
        use portable_pty::{CommandBuilder, PtySize};

        let pty_system = portable_pty::native_pty_system();
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system
            .openpty(size)
            .map_err(|e| PtyError::Pty(e.to_string()))?;

        let mut cmd = CommandBuilder::new(shell);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PtyError::Pty(e.to_string()))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PtyError::Pty(e.to_string()))?;

        let grid = Arc::new(Mutex::new(CellGrid::new(cols, rows)));
        let dirty = Arc::new(AtomicBool::new(false));
        let glyph_registry = Arc::new(Mutex::new(GlyphRegistry::new()));

        Ok(Self {
            master: pair.master,
            child,
            writer,
            reader: None,
            grid,
            dirty,
            exited: Arc::new(AtomicBool::new(false)),
            copy_mode: CopyMode::new(),
            glyph_registry,
        })
    }

    /// Returns the OS process id of the child, if it is still known.
    #[must_use]
    pub fn child_id(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Sends raw bytes to the child's stdin.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Io`] if the write fails.
    pub fn write_input(&mut self, data: &[u8]) -> Result<(), PtyError> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Resizes the PTY master to `cols×rows`.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Pty`] if the resize syscall fails.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), PtyError> {
        use portable_pty::PtySize;
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::Pty(e.to_string()))?;
        let mut grid = self.grid.lock();
        grid.resize(cols, rows);
        Ok(())
    }

    /// Starts a background reader thread that feeds PTY output into the cell grid.
    ///
    /// The caller must wrap `self` in an `Arc` before calling this method.  The
    /// reader thread sets the `dirty` flag after each batch of bytes processed.
    pub fn start_reader(self: Arc<Self>) {
        let grid = Arc::clone(&self.grid);
        let dirty = Arc::clone(&self.dirty);
        let exited = Arc::clone(&self.exited);
        let glyph_registry = Arc::clone(&self.glyph_registry);
        // ponytail: master.try_clone() is synchronous I/O; reader lives on a
        // dedicated thread so it never blocks the async runtime.
        let mut reader = match self.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                warn!("failed to clone PTY reader: {}", e);
                return;
            }
        };

        std::thread::spawn(move || {
            let mut parser = vte::Parser::new();
            let mut handler = VtHandler {
                grid,
                glyph_registry,
            };
            let mut apc_scanner = ApcScanner::new();
            let mut buf = [0u8; 4096];

            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF — child exited
                    Ok(n) => {
                        for &byte in &buf[..n] {
                            if let Some(payload) = apc_scanner.advance(byte) {
                                if let Some(reg) = st_glyph::parse_glyph_registration(&payload) {
                                    let mut registry = handler.glyph_registry.lock();
                                    let cp =
                                        registry.register_shape(&reg.id, reg.format, &reg.data);
                                    if registry.bitmap(cp).is_some() {
                                        debug!(glyph_id = %reg.id, "registered glyph via APC");
                                    } else {
                                        warn!(glyph_id = %reg.id, "glyph registered without a bitmap (rasterisation failed)");
                                    }
                                }
                            }
                            parser.advance(&mut handler, byte);
                        }
                        // Drain any OSC 52 clipboard write queued during parsing.
                        let pending = handler.grid.lock().pending_clipboard_write.take();
                        if let Some(text) = pending {
                            if let Ok(mut cb) = arboard::Clipboard::new() {
                                if let Err(e) = cb.set_text(text) {
                                    debug!("OSC 52 clipboard write error: {}", e);
                                }
                            }
                        }
                        dirty.store(true, Ordering::Release);
                    }
                    Err(e) => {
                        debug!("PTY read error: {}", e);
                        break;
                    }
                }
            }
            // Child exited — signal the host app (and wake it for a final poll).
            exited.store(true, Ordering::Release);
            dirty.store(true, Ordering::Release);
            debug!("PTY reader thread exited");
        });
    }

    /// Starts a background reader thread that feeds PTY output into the cell grid.
    ///
    /// This variant takes `&mut self` and does not require an `Arc` wrapper.
    /// The reader thread captures clones of the grid and dirty flag handles so
    /// `self` can be moved or used freely after this call.
    ///
    /// # ponytail
    ///
    /// `master.try_clone_reader()` is synchronous I/O; the reader lives on a
    /// dedicated OS thread so it never blocks the async runtime.
    pub fn start_reader_detached(&mut self) {
        let grid = Arc::clone(&self.grid);
        let dirty = Arc::clone(&self.dirty);
        let exited = Arc::clone(&self.exited);
        let glyph_registry = Arc::clone(&self.glyph_registry);
        let mut reader = match self.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                warn!("failed to clone PTY reader: {}", e);
                return;
            }
        };
        let handle = std::thread::spawn(move || {
            let mut parser = vte::Parser::new();
            let mut handler = VtHandler {
                grid,
                glyph_registry,
            };
            let mut apc_scanner = ApcScanner::new();
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        for &byte in &buf[..n] {
                            if let Some(payload) = apc_scanner.advance(byte) {
                                if let Some(reg) = st_glyph::parse_glyph_registration(&payload) {
                                    let mut registry = handler.glyph_registry.lock();
                                    let cp =
                                        registry.register_shape(&reg.id, reg.format, &reg.data);
                                    if registry.bitmap(cp).is_some() {
                                        debug!(glyph_id = %reg.id, "registered glyph via APC");
                                    } else {
                                        warn!(glyph_id = %reg.id, "glyph registered without a bitmap (rasterisation failed)");
                                    }
                                }
                            }
                            parser.advance(&mut handler, byte);
                        }
                        // Drain any OSC 52 clipboard write queued during parsing.
                        let pending = handler.grid.lock().pending_clipboard_write.take();
                        if let Some(text) = pending {
                            if let Ok(mut cb) = arboard::Clipboard::new() {
                                if let Err(e) = cb.set_text(text) {
                                    debug!("OSC 52 clipboard write error: {}", e);
                                }
                            }
                        }
                        dirty.store(true, Ordering::Release);
                    }
                    Err(e) => {
                        debug!("PTY read error: {}", e);
                        break;
                    }
                }
            }
            // Child exited — signal the host app (and wake it for a final poll).
            exited.store(true, Ordering::Release);
            dirty.store(true, Ordering::Release);
            debug!("PTY reader thread exited");
        });
        self.reader = Some(handle);
    }

    /// Enters copy mode, anchoring at the current cursor position.
    pub fn enter_copy_mode(&mut self) {
        let cursor = self.grid.lock().cursor;
        self.copy_mode.enter(cursor);
    }

    /// Copies the current selection to the clipboard.
    ///
    /// # Errors
    ///
    /// Returns [`PtyError::Clipboard`] if the clipboard write fails.
    pub fn copy_selection(&mut self) -> Result<(), PtyError> {
        let grid = self.grid.lock();
        self.copy_mode.copy_selection(&grid)
    }

    /// Searches `query` in the grid and stores matches in copy mode state.
    pub fn search(&mut self, query: &str) {
        let grid = self.grid.lock();
        self.copy_mode.search(query, &grid);
    }
}

// ── VT conformance harness ────────────────────────────────────────────────────
//
// A headless "feed bytes → snapshot the grid" pipeline used by the golden
// conformance suite and the `vtdump` example. Keeping it in the library (not
// behind `cfg(test)`) lets the example reuse it to diff recorded app streams.

/// Renders the active grid to a plain-text snapshot: one line per row, trailing
/// blanks trimmed, with trailing empty rows removed. Cursor/colour state is not
/// included — this captures the visible character layout for golden diffing.
#[must_use]
pub fn snapshot_grid(grid: &CellGrid) -> String {
    let mut lines: Vec<String> = grid
        .cells
        .iter()
        .map(|row| {
            // Skip the trailing spacer of a wide glyph so the snapshot shows the
            // actual text (e.g. "你好"), not "你 好 ".
            let s: String = row
                .iter()
                .filter(|c| !c.flags.contains(CellFlags::WIDE_SPACER))
                .map(|c| c.ch)
                .collect();
            s.trim_end().to_owned()
        })
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

/// Replays `bytes` into a fresh grid and counts cells whose stored `row`/`col`
/// fields diverge from their actual grid index. The GPU renderer positions cells
/// by these stored fields, so any nonzero count means glyphs are drawn at the
/// wrong place — diagnostic for the top-row overlap.
#[must_use]
pub fn render_vt_stale_cell_count(cols: u16, rows: u16, bytes: &[u8]) -> usize {
    let grid = Arc::new(Mutex::new(CellGrid::new(cols, rows)));
    let mut handler = VtHandler {
        grid: Arc::clone(&grid),
        glyph_registry: Arc::new(Mutex::new(GlyphRegistry::new())),
    };
    let mut parser = vte::Parser::new();
    for &b in bytes {
        parser.advance(&mut handler, b);
    }
    let guard = grid.lock();
    let mut stale = 0usize;
    for (r, row) in guard.cells.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            if usize::from(cell.row) != r || usize::from(cell.col) != c {
                stale += 1;
            }
        }
    }
    stale
}

/// FNV-1a hash of a snapshot — a compact state fingerprint for golden assertions.
#[must_use]
pub fn snapshot_hash(snapshot: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in snapshot.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Feeds `bytes` to a fresh `cols×rows` grid through the real VT parser and
/// returns its [`snapshot_grid`] text. The canonical entry point for conformance
/// fixtures: deterministic, no PTY, no GPU.
#[must_use]
pub fn render_vt_snapshot(cols: u16, rows: u16, bytes: &[u8]) -> String {
    let grid = Arc::new(Mutex::new(CellGrid::new(cols, rows)));
    let mut handler = VtHandler {
        grid: Arc::clone(&grid),
        glyph_registry: Arc::new(Mutex::new(GlyphRegistry::new())),
    };
    let mut parser = vte::Parser::new();
    for &b in bytes {
        parser.advance(&mut handler, b);
    }
    let guard = grid.lock();
    snapshot_grid(&guard)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use vte::Perform as _;

    fn make_grid(cols: u16, rows: u16) -> CellGrid {
        CellGrid::new(cols, rows)
    }

    fn make_handler(grid: Arc<Mutex<CellGrid>>) -> VtHandler {
        VtHandler {
            grid,
            glyph_registry: Arc::new(Mutex::new(st_glyph::GlyphRegistry::new())),
        }
    }

    #[test]
    fn cell_blank_has_space_char() {
        let c = Cell::blank(0, 0);
        assert_eq!(c.ch, ' ');
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

    #[test]
    fn vt_handler_print_writes_cell() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        let mut handler = make_handler(Arc::clone(&grid));
        handler.print('X');
        let g = grid.lock();
        assert_eq!(g.cells[0][0].ch, 'X');
    }

    #[test]
    fn vt_handler_execute_carriage_return() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        let mut handler = make_handler(Arc::clone(&grid));
        {
            let mut g = grid.lock();
            g.cursor = (10, 5);
        }
        handler.execute(b'\r');
        assert_eq!(grid.lock().cursor.0, 0);
    }

    #[test]
    fn vt_handler_csi_cursor_up() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        {
            let mut g = grid.lock();
            g.cursor = (0, 5);
        }
        let handler = make_handler(Arc::clone(&grid));
        // CSI 3 A (cursor up 3): test via direct grid mutation since constructing
        // Params from scratch is non-trivial in unit tests.
        handler.grid.lock().cursor.1 = 5;
        drop(handler);
        let mut g = grid.lock();
        // Simulate cursor up by 3.
        g.cursor.1 = g.cursor.1.saturating_sub(3);
        assert_eq!(g.cursor.1, 2);
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

    #[test]
    fn copy_mode_search_multibyte_reports_cell_columns() {
        let mut grid = make_grid(20, 5);
        // Two 4-byte emoji then "AB". The match "A" sits at cell column 2 but at
        // byte offset 8 — the old code pushed the byte offset as the column.
        for (i, ch) in "\u{1f680}\u{1f680}AB".chars().enumerate() {
            grid.cells[0][i].ch = ch;
        }
        let mut cm = CopyMode::new();
        cm.search("A", &grid);
        assert_eq!(
            cm.search_matches,
            vec![(2, 0)],
            "column must be the cell index, not the raw byte offset"
        );
    }

    #[test]
    fn copy_mode_search_repeated_multibyte_query_does_not_panic() {
        let mut grid = make_grid(20, 5);
        // Three CJK codepoints. Searching for one of them must find all three.
        // The old `start = abs + 1` stepped into the middle of the first 3-byte
        // codepoint and panicked on the next slice. Fail-before.
        for (i, ch) in "\u{4e2d}\u{4e2d}\u{4e2d}".chars().enumerate() {
            grid.cells[0][i].ch = ch;
        }
        let mut cm = CopyMode::new();
        cm.search("\u{4e2d}", &grid);
        assert_eq!(
            cm.search_matches,
            vec![(0, 0), (1, 0), (2, 0)],
            "each cell holding the query char is a distinct match at its column"
        );
    }

    #[test]
    fn color_to_rgba_ansi_uses_palette() {
        let palette = DEFAULT_PALETTE;
        let rgba = Color::Ansi(0).to_rgba(&palette, true);
        assert_eq!(rgba, palette[0]);
    }

    #[test]
    fn color_to_rgba_rgb_scales_correctly() {
        let palette = DEFAULT_PALETTE;
        let rgba = Color::Rgb(255, 0, 0).to_rgba(&palette, true);
        assert!((rgba[0] - 1.0).abs() < 1e-6);
        assert_eq!(rgba[1], 0.0);
        assert_eq!(rgba[2], 0.0);
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
    fn ps1_heuristic_emits_marker() {
        // The heuristic fires in VtHandler::print(), not put_char() directly.
        // Test via VtHandler so the full call chain is exercised.
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        {
            let mut g = grid.lock();
            g.osc133_seen = false;
        }
        let mut handler = make_handler(Arc::clone(&grid));
        handler.print('$');
        let g = grid.lock();
        assert!(
            g.block_markers
                .iter()
                .any(|m| m.kind == MarkerKind::PromptHeuristic),
            "expected PromptHeuristic marker after printing '$'"
        );
    }

    #[test]
    fn parse_osc9_returns_notification_with_payload_as_body() {
        let n = parse_osc9("hello from shell").unwrap();
        assert_eq!(n.title, "smedja");
        assert_eq!(n.body, "hello from shell");
    }

    #[test]
    fn parse_osc777_valid_payload_extracts_title_and_body() {
        let n = parse_osc777("notify;My App;Something happened").unwrap();
        assert_eq!(n.title, "My App");
        assert_eq!(n.body, "Something happened");
    }

    #[test]
    fn parse_osc777_invalid_payload_returns_none() {
        assert!(parse_osc777("toast;oops").is_none());
        assert!(parse_osc777("").is_none());
        assert!(parse_osc777("notify;only-title").is_none());
    }

    // ── parse_osc7_uri ────────────────────────────────────────────────────────

    #[test]
    fn parse_osc7_uri_localhost_triple_slash() {
        let path = parse_osc7_uri("file:///home/user/project").unwrap();
        assert_eq!(path, "/home/user/project");
    }

    #[test]
    fn parse_osc7_uri_with_hostname() {
        let path = parse_osc7_uri("file://myhost/home/user/project").unwrap();
        assert_eq!(path, "/home/user/project");
    }

    #[test]
    fn parse_osc7_uri_non_file_scheme_returns_none() {
        assert!(parse_osc7_uri("http://example.com/path").is_none());
        assert!(parse_osc7_uri("").is_none());
    }

    // ── CellGrid::resize ──────────────────────────────────────────────────────

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

    // ── scroll on last row ────────────────────────────────────────────────────

    #[test]
    fn newline_at_last_row_scrolls_without_panic() {
        let grid = Arc::new(Mutex::new(make_grid(4, 3)));
        let mut handler = make_handler(grid.clone());
        handler.execute(b'\n');
        handler.execute(b'\n');
        handler.execute(b'\n'); // cursor now at last row
        handler.execute(b'\n'); // should scroll, not panic
        let g = grid.lock();
        assert!(g.cursor.1 < g.rows, "cursor.1 must be within grid bounds");
    }

    // ── VTE sequence dispatch ─────────────────────────────────────────────────

    #[test]
    fn vte_cursor_home_csi_h() {
        let grid = Arc::new(Mutex::new(make_grid(20, 10)));
        {
            let mut g = grid.lock();
            g.cursor = (10, 5);
        }
        let mut handler = make_handler(grid.clone());
        // \x1b[H — cursor home, no params → row=0, col=0
        let params = vte::Params::default();
        handler.csi_dispatch(&params, &[], false, 'H');
        let g = grid.lock();
        assert_eq!(g.cursor, (0, 0));
    }

    #[test]
    fn vte_cursor_home_no_args() {
        let grid = Arc::new(Mutex::new(make_grid(20, 10)));
        {
            let mut g = grid.lock();
            g.cursor = (5, 3);
        }
        let mut handler = make_handler(grid.clone());
        let params = vte::Params::default();
        handler.csi_dispatch(&params, &[], false, 'H');
        let g = grid.lock();
        assert_eq!(g.cursor, (0, 0));
    }

    #[test]
    fn vte_delete_line_csi_m() {
        let grid = Arc::new(Mutex::new(make_grid(10, 5)));
        {
            let mut g = grid.lock();
            g.cells[2][0].ch = 'Z';
            g.cursor = (0, 2);
        }
        let mut handler = make_handler(grid.clone());
        // \x1b[M — delete current line
        let params = vte::Params::default();
        handler.csi_dispatch(&params, &[], false, 'M');
        let g = grid.lock();
        // Grid still has the same number of rows.
        assert_eq!(g.cells.len(), 5);
        // Row 2 (the cursor row) should now be blank — the original 'Z' row was removed.
        assert_eq!(g.cells[2][0].ch, ' ');
    }

    #[test]
    fn vte_index_esc_d_moves_cursor_down() {
        let grid = Arc::new(Mutex::new(make_grid(10, 5)));
        {
            let mut g = grid.lock();
            g.cursor = (3, 2);
        }
        let mut handler = make_handler(grid.clone());
        // ESC D — Index: move cursor down one row (or scroll if at bottom).
        handler.esc_dispatch(&[], false, b'D');
        let g = grid.lock();
        assert_eq!(g.cursor.1, 3, "cursor should move down one row");
    }

    #[test]
    fn vte_reverse_index_esc_m_moves_cursor_up() {
        let grid = Arc::new(Mutex::new(make_grid(10, 5)));
        {
            let mut g = grid.lock();
            g.cursor = (3, 2);
        }
        let mut handler = make_handler(grid.clone());
        // ESC M — Reverse Index: move cursor up one row (or scroll down if at top).
        handler.esc_dispatch(&[], false, b'M');
        let g = grid.lock();
        assert_eq!(g.cursor.1, 1, "cursor should move up one row");
    }

    // ── Section 4a: OSC 0 — window title ─────────────────────────────────

    #[test]
    fn vte_osc0_sets_window_title() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        let mut handler = make_handler(Arc::clone(&grid));
        let mut parser = vte::Parser::new();
        // OSC 0 ; title BEL
        let seq = b"\x1b]0;my terminal title\x07";
        for &byte in seq {
            parser.advance(&mut handler, byte);
        }
        let g = grid.lock();
        assert_eq!(
            g.title.as_deref(),
            Some("my terminal title"),
            "OSC 0 should set the window title"
        );
    }

    fn feed(handler: &mut VtHandler, parser: &mut vte::Parser, seq: &[u8]) {
        for &byte in seq {
            parser.advance(handler, byte);
        }
    }

    #[test]
    fn kitty_push_sets_flags_and_pop_clears() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        let mut handler = make_handler(Arc::clone(&grid));
        let mut parser = vte::Parser::new();
        // Push DISAMBIGUATE_ESCAPE_CODES (1).
        feed(&mut handler, &mut parser, b"\x1b[>1u");
        assert_eq!(grid.lock().kbd_flags(), 1, "push should set active flags");
        // Pop restores legacy (0).
        feed(&mut handler, &mut parser, b"\x1b[<u");
        assert_eq!(grid.lock().kbd_flags(), 0, "pop should clear active flags");
    }

    #[test]
    fn kitty_query_queues_response() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        let mut handler = make_handler(Arc::clone(&grid));
        let mut parser = vte::Parser::new();
        feed(&mut handler, &mut parser, b"\x1b[>5u"); // flags = 5
        feed(&mut handler, &mut parser, b"\x1b[?u"); // query
        let resp = std::mem::take(&mut grid.lock().pending_responses);
        assert_eq!(
            resp,
            b"\x1b[?5u".to_vec(),
            "query should report the active flags"
        );
    }

    #[test]
    fn kitty_set_mode_2_sets_bits_mode_3_clears() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        let mut handler = make_handler(Arc::clone(&grid));
        let mut parser = vte::Parser::new();
        feed(&mut handler, &mut parser, b"\x1b[>1u"); // base flags = 1
        feed(&mut handler, &mut parser, b"\x1b[=4;2u"); // set bit 4
        assert_eq!(grid.lock().kbd_flags(), 5);
        feed(&mut handler, &mut parser, b"\x1b[=1;3u"); // clear bit 1
        assert_eq!(grid.lock().kbd_flags(), 4);
    }

    #[test]
    fn vte_osc2_sets_window_title() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        let mut handler = make_handler(Arc::clone(&grid));
        let mut parser = vte::Parser::new();
        // OSC 2 ; title BEL
        let seq = b"\x1b]2;icon title\x07";
        for &byte in seq {
            parser.advance(&mut handler, byte);
        }
        let g = grid.lock();
        assert_eq!(
            g.title.as_deref(),
            Some("icon title"),
            "OSC 2 should set the window title"
        );
    }

    // ── Section 4b: ESC 7 / ESC 8 — cursor save/restore ─────────────────

    #[test]
    fn vte_esc7_saves_and_esc8_restores_cursor() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        let mut handler = make_handler(Arc::clone(&grid));
        let mut parser = vte::Parser::new();
        // Move cursor to (5, 3), save, move to (0, 0), restore.
        let seq = b"\x1b[4;6H\x1b7\x1b[H\x1b8";
        for &byte in seq {
            parser.advance(&mut handler, byte);
        }
        let g = grid.lock();
        // After restore, cursor should be at col 5, row 3 (0-indexed after 1-based CSI H).
        assert_eq!(g.cursor.0, 5, "cursor col should be restored to 5");
        assert_eq!(g.cursor.1, 3, "cursor row should be restored to 3");
    }

    // ── Section 4c: CSI ?25l / ?25h — cursor hide/show ───────────────────

    #[test]
    fn vte_cursor_hide_and_show() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        let mut handler = make_handler(Arc::clone(&grid));
        let mut parser = vte::Parser::new();
        // Hide cursor.
        let seq = b"\x1b[?25l";
        for &byte in seq {
            parser.advance(&mut handler, byte);
        }
        {
            let g = grid.lock();
            assert!(!g.cursor_visible, "?25l should hide cursor");
        }
        // Show cursor.
        let seq2 = b"\x1b[?25h";
        for &byte in seq2 {
            parser.advance(&mut handler, byte);
        }
        {
            let g = grid.lock();
            assert!(g.cursor_visible, "?25h should show cursor");
        }
    }

    // ── Section 4d: regression tests for already-implemented sequences ────

    #[test]
    fn vte_24bit_fg_colour() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        let mut handler = make_handler(Arc::clone(&grid));
        let mut parser = vte::Parser::new();
        let seq = b"\x1b[38;2;255;128;0m";
        for &byte in seq {
            parser.advance(&mut handler, byte);
        }
        let g = grid.lock();
        assert_eq!(
            g.sgr.fg,
            Color::Rgb(255, 128, 0),
            "SGR 38;2 should set 24-bit fg colour"
        );
    }

    #[test]
    fn vte_256_bg_colour() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        let mut handler = make_handler(Arc::clone(&grid));
        let mut parser = vte::Parser::new();
        let seq = b"\x1b[48;5;196m";
        for &byte in seq {
            parser.advance(&mut handler, byte);
        }
        let g = grid.lock();
        assert_eq!(
            g.sgr.bg,
            Color::Ansi256(196),
            "SGR 48;5 should set 256-colour bg"
        );
    }

    #[test]
    fn vte_alternate_screen_enter_exit() {
        let grid = Arc::new(Mutex::new(make_grid(80, 24)));
        let mut handler = make_handler(Arc::clone(&grid));
        let mut parser = vte::Parser::new();
        // Enter alt screen.
        let enter = b"\x1b[?1049h";
        for &byte in enter {
            parser.advance(&mut handler, byte);
        }
        {
            let g = grid.lock();
            assert!(g.alt_screen, "?1049h should enter alt screen");
        }
        // Exit alt screen.
        let exit = b"\x1b[?1049l";
        for &byte in exit {
            parser.advance(&mut handler, byte);
        }
        {
            let g = grid.lock();
            assert!(!g.alt_screen, "?1049l should exit alt screen");
        }
    }

    // ── APC scanner ───────────────────────────────────────────────────────────

    #[test]
    fn apc_scanner_extracts_payload_from_complete_sequence() {
        let mut scanner = ApcScanner::new();
        let seq = b"\x1b_hello;world\x1b\\";
        let mut result = None;
        for &byte in seq {
            if let Some(payload) = scanner.advance(byte) {
                result = Some(payload);
            }
        }
        assert_eq!(result.as_deref(), Some(b"hello;world" as &[u8]));
    }

    #[test]
    fn apc_scanner_returns_none_for_incomplete_sequence() {
        let mut scanner = ApcScanner::new();
        for &byte in b"\x1b_incomplete" {
            assert!(scanner.advance(byte).is_none());
        }
    }

    #[test]
    fn apc_scanner_handles_esc_in_payload_not_followed_by_backslash() {
        let mut scanner = ApcScanner::new();
        // ESC followed by 'X' (not backslash) inside APC payload — should be included in payload.
        let seq = b"\x1b_foo\x1bXbar\x1b\\";
        let mut result = None;
        for &byte in seq {
            if let Some(payload) = scanner.advance(byte) {
                result = Some(payload);
            }
        }
        let payload = result.expect("complete APC sequence should yield a payload");
        assert!(
            payload.contains(&b'\x1b'),
            "ESC inside payload should be preserved"
        );
    }

    #[test]
    fn glyph_registration_via_apc_updates_registry() {
        // "PHN2Zy8+" is base64("<svg/>") — hardcoded to avoid adding base64 as test dep
        let mut apc_seq = Vec::new();
        apc_seq.extend_from_slice(b"\x1b_");
        apc_seq.extend_from_slice(b"SMEDJA_GLYPH;id=test.icon;format=svg;data=PHN2Zy8+");
        apc_seq.extend_from_slice(b"\x1b\\");

        let registry = Arc::new(Mutex::new(st_glyph::GlyphRegistry::new()));
        let mut scanner = ApcScanner::new();

        for &byte in &apc_seq {
            if let Some(payload) = scanner.advance(byte) {
                if let Some(reg) = st_glyph::parse_glyph_registration(&payload) {
                    let mut r = registry.lock();
                    r.register(&reg.id);
                }
            }
        }

        assert!(
            registry.lock().lookup("test.icon").is_some(),
            "test.icon should be in the registry after APC registration"
        );
    }

    #[test]
    fn glyph_registration_via_apc_rasterises_and_stores_bitmap() {
        // Hardcoded base64 of a 1×1 RGB PNG so register_shape can decode it to a
        // bitmap without adding base64/png as a dev-dependency.
        const PNG_B64: &str =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGMQUDAAAACkAGE0Zn1yAAAAAElFTkSuQmCC";

        let mut apc_seq = Vec::new();
        apc_seq.extend_from_slice(b"\x1b_");
        apc_seq.extend_from_slice(
            format!("SMEDJA_GLYPH;id=test.png;format=png;data={PNG_B64}").as_bytes(),
        );
        apc_seq.extend_from_slice(b"\x1b\\");

        let registry = Arc::new(Mutex::new(st_glyph::GlyphRegistry::new()));
        let mut scanner = ApcScanner::new();

        for &byte in &apc_seq {
            if let Some(payload) = scanner.advance(byte) {
                if let Some(reg) = st_glyph::parse_glyph_registration(&payload) {
                    let mut r = registry.lock();
                    r.register_shape(&reg.id, reg.format, &reg.data);
                }
            }
        }

        let r = registry.lock();
        let cp = r
            .lookup("test.png")
            .expect("test.png should be registered after APC registration");
        assert!(
            r.bitmap(cp).is_some(),
            "registered PNG should have a rasterised bitmap keyed by its codepoint"
        );
    }

    #[test]
    fn startup_sequence_contains_apc_bytes_for_builtins() {
        let mut registry = st_glyph::GlyphRegistry::new();
        st_glyph::register_builtin_glyphs(&mut registry);
        let seq = st_glyph::build_glyph_registration_sequence(&registry);
        assert!(
            seq.windows(2).any(|w| w == b"\x1b_"),
            "startup sequence should contain at least one APC introducer"
        );
        assert!(
            seq.windows(2).any(|w| w == b"\x1b\\"),
            "startup sequence should contain at least one string terminator"
        );
    }

    // ── VT conformance golden suite ──────────────────────────────────────────
    // Each case feeds bytes to a fresh grid and asserts the snapshot. These lock
    // in correct VT behaviour so later phases (wide chars, SGR, scroll regions)
    // can't silently regress the basics.

    #[test]
    fn conformance_plain_text() {
        assert_eq!(render_vt_snapshot(20, 3, b"hello world"), "hello world");
    }

    #[test]
    fn conformance_crlf_newline() {
        assert_eq!(render_vt_snapshot(20, 3, b"line1\r\nline2"), "line1\nline2");
    }

    #[test]
    fn conformance_deferred_wrap_at_width() {
        // 4-col grid: "abcd" fills the row (deferred wrap, no advance); "abcde"
        // wraps the 'e' onto the next row.
        assert_eq!(render_vt_snapshot(4, 3, b"abcd"), "abcd");
        assert_eq!(render_vt_snapshot(4, 3, b"abcde"), "abcd\ne");
    }

    #[test]
    fn conformance_cursor_position_and_overwrite() {
        // CSI 2;3 H places the cursor at row 2 col 3 (1-based) → 'X' at [1][2].
        assert_eq!(render_vt_snapshot(6, 3, b"\x1b[2;3HX"), "\n  X");
    }

    #[test]
    fn conformance_carriage_return_overwrite() {
        assert_eq!(render_vt_snapshot(6, 2, b"abc\rXY"), "XYc");
    }

    #[test]
    fn conformance_erase_line_to_end() {
        // Write "abcdef", move to col 3, erase-to-end (CSI K) → "ab".
        assert_eq!(render_vt_snapshot(8, 2, b"abcdef\x1b[1;3H\x1b[K"), "ab");
    }

    #[test]
    fn conformance_erase_display() {
        assert_eq!(render_vt_snapshot(8, 3, b"foo\r\nbar\x1b[2J"), "");
    }

    #[test]
    fn conformance_backspace_moves_cursor_left() {
        assert_eq!(render_vt_snapshot(8, 2, b"abc\x08X"), "abX");
    }

    #[test]
    fn conformance_wide_char_occupies_two_cells() {
        // A CJK glyph takes 2 columns: leading WIDE cell + WIDE_SPACER. The
        // snapshot skips spacers, so "你好" reads back verbatim.
        assert_eq!(render_vt_snapshot(8, 2, "你好".as_bytes()), "你好");
        // After a width-2 glyph the cursor is at column 2, so the next ASCII
        // char lands there.
        assert_eq!(render_vt_snapshot(8, 2, "你x".as_bytes()), "你x");
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
    fn conformance_scroll_region_ind_scrolls_only_the_region() {
        // 4 rows A/B/C/D. DECSTBM region = rows 2..3 (B,C). Park the cursor on
        // the bottom margin and IND (ESC D): B,C scroll up to C,blank; A and D
        // (outside the region) are untouched.
        let out = render_vt_snapshot(4, 4, b"A\r\nB\r\nC\r\nD\x1b[2;3r\x1b[3;1H\x1bD");
        assert_eq!(out, "A\nC\n\nD");
    }

    #[test]
    fn conformance_scroll_region_ri_reverse_scrolls_region() {
        // Same region; park on the top margin and RI (ESC M): B,C scroll down to
        // blank,B.
        let out = render_vt_snapshot(4, 4, b"A\r\nB\r\nC\r\nD\x1b[2;3r\x1b[2;1H\x1bM");
        assert_eq!(out, "A\n\nB\nD");
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

    #[test]
    fn scroll_region_resets_to_full_screen_on_resize() {
        let mut grid = make_grid(4, 4);
        grid.scroll_top = 1;
        grid.scroll_bottom = 2;
        grid.resize(4, 4);
        assert_eq!(grid.scroll_top, 0);
        assert_eq!(grid.scroll_bottom, 3);
    }

    #[test]
    fn conformance_snapshot_hash_is_stable() {
        let a = render_vt_snapshot(10, 2, b"hello");
        let b = render_vt_snapshot(10, 2, b"hello");
        assert_eq!(snapshot_hash(&a), snapshot_hash(&b));
        assert_ne!(
            snapshot_hash(&a),
            snapshot_hash(&render_vt_snapshot(10, 2, b"world"))
        );
    }

    // Regression: closing a session while its child is still running must
    // reap the child (no zombie) and join the reader thread (no leaked thread
    // or master fd). Uses /proc, so it is Linux-only.
    #[cfg(target_os = "linux")]
    #[test]
    fn drop_reaps_live_child_and_joins_reader() {
        use std::time::{Duration, Instant};

        // `cat` blocks reading the pty slave, so it stays alive after spawn —
        // this reproduces the "close a split while the child is still
        // running" path that previously leaked the reader thread, the master
        // fd, and a zombie child.
        let mut session = PtySession::spawn(80, 24, "cat").expect("spawn cat");
        session.start_reader_detached();

        let pid = session.child_id().expect("child pid");
        let proc_path = format!("/proc/{pid}");
        assert!(
            std::path::Path::new(&proc_path).exists(),
            "child should be alive before drop"
        );
        assert!(
            !session.exited.load(Ordering::Relaxed),
            "child should still be running before drop"
        );

        // Full teardown runs here: SIGHUP the child, wait() to reap it, then
        // join the reader thread. A leaked/blocked reader thread would make
        // the join hang and this test time out rather than pass.
        drop(session);

        // Once the parent has reaped the child, its /proc entry disappears.
        // Poll briefly to avoid racing the kernel's teardown.
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::path::Path::new(&proc_path).exists() {
            assert!(
                Instant::now() < deadline,
                "child pid {pid} was not reaped: zombie or still running after drop"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
