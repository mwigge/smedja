//! Terminal-local mouse text selection: range maths and text extraction.
//!
//! Kept free of `App`/winit state so the event handlers stay thin and the
//! logic is unit-testable. Coordinates are visible-grid `(col, row)` cells —
//! the same space `App::pointer_cell` reports in.

/// One endpoint of a selection in visible-grid cell coordinates `(col, row)`.
pub(crate) type CellPos = (u16, u16);

/// Normalises a selection so `start` is the reading-order first endpoint.
pub(crate) fn normalize(a: CellPos, b: CellPos) -> (CellPos, CellPos) {
    if (a.1, a.0) <= (b.1, b.0) {
        (a, b)
    } else {
        (b, a)
    }
}

/// True when cell `(col, row)` falls inside the normalised range.
pub(crate) fn contains(range: &(CellPos, CellPos), col: u16, row: u16) -> bool {
    let ((c1, r1), (c2, r2)) = *range;
    if row < r1 || row > r2 {
        return false;
    }
    if r1 == r2 {
        return c1 <= col && col <= c2;
    }
    if row == r1 {
        return col >= c1;
    }
    if row == r2 {
        return col <= c2;
    }
    true
}

/// Extracts the selected text from the visible rows (the same view the
/// redraw path draws). Wide-char spacer cells contribute nothing; trailing
/// whitespace is trimmed per line and trailing blank lines are dropped.
pub(crate) fn extract_text(rows: &[&Vec<st_pty::Cell>], range: &(CellPos, CellPos)) -> String {
    let ((_, r1), (_, r2)) = *range;
    let mut lines: Vec<String> = Vec::new();
    for (r, row_cells) in rows.iter().enumerate() {
        let Ok(rr) = u16::try_from(r) else { break };
        if rr < r1 {
            continue;
        }
        if rr > r2 {
            break;
        }
        let mut line = String::new();
        for (c, cell) in row_cells.iter().enumerate() {
            let Ok(cc) = u16::try_from(c) else { break };
            if contains(range, cc, rr) && !cell.flags.contains(st_pty::CellFlags::WIDE_SPACER) {
                line.push(cell.ch);
            }
        }
        lines.push(line.trim_end().to_owned());
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_of(text: &str, row: u16, width: u16) -> Vec<st_pty::Cell> {
        let mut cells: Vec<st_pty::Cell> =
            (0..width).map(|c| st_pty::Cell::blank(c, row)).collect();
        for (c, ch) in text.chars().enumerate() {
            if let Some(cell) = cells.get_mut(c) {
                cell.ch = ch;
            }
        }
        cells
    }

    #[test]
    fn normalize_orders_reversed_endpoints() {
        assert_eq!(normalize((5, 3), (2, 1)), ((2, 1), (5, 3)));
        assert_eq!(normalize((2, 1), (5, 3)), ((2, 1), (5, 3)));
        // Same row: column order decides.
        assert_eq!(normalize((7, 2), (4, 2)), ((4, 2), (7, 2)));
    }

    #[test]
    fn contains_single_row_range() {
        let range = ((2u16, 1u16), (5u16, 1u16));
        assert!(!contains(&range, 1, 1));
        assert!(contains(&range, 2, 1));
        assert!(contains(&range, 5, 1));
        assert!(!contains(&range, 6, 1));
        assert!(!contains(&range, 3, 0));
    }

    #[test]
    fn contains_multi_row_range() {
        let range = ((3u16, 1u16), (2u16, 3u16));
        assert!(contains(&range, 3, 1));
        assert!(!contains(&range, 2, 1)); // first row: from c1 rightwards
        assert!(contains(&range, 0, 2)); // middle rows: full width
        assert!(contains(&range, 2, 3));
        assert!(!contains(&range, 3, 3)); // last row: up to c2
        assert!(!contains(&range, 0, 4));
    }

    #[test]
    fn extract_single_row_trims_trailing_blanks() {
        let rows = [row_of("hello", 0, 20)];
        let refs: Vec<&Vec<st_pty::Cell>> = rows.iter().collect();
        let text = extract_text(&refs, &((1, 0), (19, 0)));
        assert_eq!(text, "ello");
    }

    #[test]
    fn extract_multi_row_joins_with_newlines() {
        let rows = [row_of("abc", 0, 10), row_of("def", 1, 10)];
        let refs: Vec<&Vec<st_pty::Cell>> = rows.iter().collect();
        let text = extract_text(&refs, &((1, 0), (2, 1)));
        assert_eq!(text, "bc\ndef");
    }

    #[test]
    fn extract_skips_wide_spacer_cells() {
        let mut cells = row_of("a", 0, 6);
        // A double-width glyph at col 1: leading cell carries the char, the
        // trailing spacer must not leak a blank into the copied text.
        cells[1].ch = '世';
        cells[1].flags |= st_pty::CellFlags::WIDE;
        cells[2].flags |= st_pty::CellFlags::WIDE_SPACER;
        cells[3].ch = 'b';
        let rows = [cells];
        let refs: Vec<&Vec<st_pty::Cell>> = rows.iter().collect();
        let text = extract_text(&refs, &((0, 0), (5, 0)));
        assert_eq!(text, "a世b");
    }

    #[test]
    fn extract_drops_trailing_blank_lines() {
        let rows = [row_of("abc", 0, 10), row_of("", 1, 10), row_of("", 2, 10)];
        let refs: Vec<&Vec<st_pty::Cell>> = rows.iter().collect();
        let text = extract_text(&refs, &((0, 0), (9, 2)));
        assert_eq!(text, "abc");
    }
}
