/// Converts a PTY grid cell into a renderer cell at `(col, row)`, resolving the
/// style flags before handing the cell to `st-render`.
pub(crate) fn render_cell(c: &st_pty::Cell, col: u16, row: u16) -> st_render::Cell {
    use st_pty::CellFlags;
    let f = c.flags;
    let (mut fg, mut bg) = (c.fg, c.bg);
    if f.contains(CellFlags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if f.contains(CellFlags::DIM) {
        for ch in fg.iter_mut().take(3) {
            *ch *= 0.6;
        }
    }
    st_render::Cell {
        ch: c.ch,
        fg,
        bg,
        col,
        row,
        bold: f.contains(CellFlags::BOLD),
        italic: f.contains(CellFlags::ITALIC),
        underline: f.contains(CellFlags::UNDERLINE),
        strikethrough: f.contains(CellFlags::STRIKETHROUGH),
        wide: f.contains(CellFlags::WIDE),
    }
}
