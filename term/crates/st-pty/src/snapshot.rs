//! VT conformance harness: headless "feed bytes → snapshot the grid" helpers.
//!
//! A headless pipeline used by the golden conformance suite and the `vtdump`
//! example. Keeping it in the library (not behind `cfg(test)`) lets the example
//! reuse it to diff recorded app streams.

use std::sync::Arc;

use parking_lot::Mutex;

use st_glyph::GlyphRegistry;

use crate::cell::CellFlags;
use crate::grid::CellGrid;
use crate::vt::VtHandler;

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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn conformance_snapshot_hash_is_stable() {
        let a = render_vt_snapshot(10, 2, b"hello");
        let b = render_vt_snapshot(10, 2, b"hello");
        assert_eq!(snapshot_hash(&a), snapshot_hash(&b));
        assert_ne!(
            snapshot_hash(&a),
            snapshot_hash(&render_vt_snapshot(10, 2, b"world"))
        );
    }
}
