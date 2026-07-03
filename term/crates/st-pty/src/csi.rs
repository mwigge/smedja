//! CSI (Control Sequence Introducer) dispatch.

use tracing::debug;

use crate::grid::{blank_row, CellGrid};
use crate::mouse::MouseMode;
use crate::sgr::apply_sgr;

/// Handles a CSI sequence, mutating `grid` accordingly.
#[allow(clippy::too_many_lines)] // complex VT dispatch is inherently long
pub(crate) fn dispatch(
    grid: &mut CellGrid,
    params: &vte::Params,
    intermediates: &[u8],
    action: char,
) {
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
            apply_sgr(grid, &p);
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

#[cfg(test)]
mod tests {
    use crate::color::Color;
    use crate::grid::CellGrid;
    use crate::vt::VtHandler;
    use parking_lot::Mutex;
    use std::sync::Arc;
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

    fn feed(handler: &mut VtHandler, parser: &mut vte::Parser, seq: &[u8]) {
        for &byte in seq {
            parser.advance(handler, byte);
        }
    }

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
}
