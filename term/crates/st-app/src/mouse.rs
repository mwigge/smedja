pub(crate) fn encode_mouse_sgr(col: u16, row: u16, button: u8, pressed: bool) -> Vec<u8> {
    let suffix = if pressed { b'M' } else { b'm' };
    format!("\x1b[<{};{};{}{}", button, col + 1, row + 1, suffix as char).into_bytes()
}

pub(crate) fn encode_mouse_x10(col: u16, row: u16, button: u8) -> Vec<u8> {
    let cb = button.saturating_add(32);
    // Clamp in u16 space first to avoid silent truncation when col/row >= 255.
    let cx = (col + 1).min(223) as u8 + 32;
    let cy = (row + 1).min(223) as u8 + 32;
    vec![b'\x1b', b'[', b'M', cb, cx, cy]
}
