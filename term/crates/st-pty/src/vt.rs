//! The VT / escape-sequence parser: the `vte::Perform` state machine, the APC
//! pre-scanner, SGR application, and the headless conformance harness.

use std::sync::Arc;

use parking_lot::Mutex;
use tracing::debug;

use st_glyph::GlyphRegistry;

use crate::cell::CellFlags;
use crate::color::Color;
use crate::grid::{
    blank_row, parse_osc777, parse_osc7_uri, parse_osc9, BlockMarker, CellGrid, MarkerKind,
    MouseMode,
};

// ── APC pre-scanner ───────────────────────────────────────────────────────────

/// State machine that scans raw PTY bytes for `ESC _ … ESC \` (APC) sequences.
///
/// vte 0.13 routes APC bytes to its `Ignore` state and never fires a
/// performer callback, so this scanner runs alongside the vte parser to
/// intercept smedja Glyph Protocol registrations emitted by child processes.
#[derive(Debug, Default)]
pub(crate) struct ApcScanner {
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
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feeds one byte into the scanner.
    ///
    /// Returns the completed APC payload bytes when a full `ESC _ … ESC \`
    /// sequence has been received, or `None` otherwise.
    pub(crate) fn advance(&mut self, byte: u8) -> Option<Vec<u8>> {
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

pub(crate) struct VtHandler {
    pub(crate) grid: Arc<Mutex<CellGrid>>,
    pub(crate) glyph_registry: Arc<Mutex<GlyphRegistry>>,
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
