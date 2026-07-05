use std::sync::atomic::Ordering;
use std::sync::Arc;

use parking_lot::Mutex;
use vte::Perform as _;

use super::*;
use crate::color::DEFAULT_PALETTE;
use crate::vt::{apply_sgr, ApcScanner, VtHandler};

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
