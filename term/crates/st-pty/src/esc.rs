//! ESC (escape) sequence dispatch.

use tracing::debug;

use crate::grid::CellGrid;

/// Handles a plain ESC sequence identified by its final `byte`.
pub(crate) fn dispatch(grid: &mut CellGrid, byte: u8) {
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

#[cfg(test)]
mod tests {
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
}
