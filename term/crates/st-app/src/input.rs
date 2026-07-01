use winit::keyboard::{Key, NamedKey};
pub(crate) fn key_to_pty_bytes(key: &Key) -> Option<Vec<u8>> {
    match key {
        Key::Character(s) => Some(s.as_str().as_bytes().to_vec()),
        Key::Named(NamedKey::Space) => Some(b" ".to_vec()),
        Key::Named(NamedKey::Enter) => Some(b"\r".to_vec()),
        Key::Named(NamedKey::Backspace) => Some(b"\x7f".to_vec()),
        Key::Named(NamedKey::Tab) => Some(b"\t".to_vec()),
        Key::Named(NamedKey::Escape) => Some(b"\x1b".to_vec()),
        Key::Named(NamedKey::ArrowUp) => Some(b"\x1b[A".to_vec()),
        Key::Named(NamedKey::ArrowDown) => Some(b"\x1b[B".to_vec()),
        Key::Named(NamedKey::ArrowRight) => Some(b"\x1b[C".to_vec()),
        Key::Named(NamedKey::ArrowLeft) => Some(b"\x1b[D".to_vec()),
        Key::Named(NamedKey::Home) => Some(b"\x1b[H".to_vec()),
        Key::Named(NamedKey::End) => Some(b"\x1b[F".to_vec()),
        Key::Named(NamedKey::Delete) => Some(b"\x1b[3~".to_vec()),
        Key::Named(NamedKey::PageUp) => Some(b"\x1b[5~".to_vec()),
        Key::Named(NamedKey::PageDown) => Some(b"\x1b[6~".to_vec()),
        _ => None,
    }
}

fn kitty_modifier(shift: bool, alt: bool, ctrl: bool, sup: bool) -> u8 {
    let mut bits = 0u8;
    if shift {
        bits |= 1;
    }
    if alt {
        bits |= 2;
    }
    if ctrl {
        bits |= 4;
    }
    if sup {
        bits |= 8;
    }
    1 + bits
}

fn kitty_functional_codepoint(named: &NamedKey) -> Option<u32> {
    Some(match named {
        NamedKey::Enter => 13,
        NamedKey::Tab => 9,
        NamedKey::Backspace => 127,
        NamedKey::Escape => 27,
        NamedKey::Space => 32,
        _ => return None,
    })
}

fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        ' ' | '@' => Some(0x00),
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' | '/' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

fn modified_named_legacy(named: &NamedKey, modifier: u8) -> Option<Vec<u8>> {
    let cursor = |final_byte: char| Some(format!("\x1b[1;{modifier}{final_byte}").into_bytes());
    let tilde = |n: u8| Some(format!("\x1b[{n};{modifier}~").into_bytes());
    match named {
        NamedKey::ArrowUp => cursor('A'),
        NamedKey::ArrowDown => cursor('B'),
        NamedKey::ArrowRight => cursor('C'),
        NamedKey::ArrowLeft => cursor('D'),
        NamedKey::Home => cursor('H'),
        NamedKey::End => cursor('F'),
        NamedKey::Delete => tilde(3),
        NamedKey::PageUp => tilde(5),
        NamedKey::PageDown => tilde(6),
        _ => None,
    }
}

pub(crate) fn encode_key(
    key: &Key,
    shift: bool,
    alt: bool,
    ctrl: bool,
    sup: bool,
    kbd_flags: u8,
) -> Option<Vec<u8>> {
    let any_mod = shift || alt || ctrl || sup;
    let modifier = kitty_modifier(shift, alt, ctrl, sup);

    // ── Kitty enhanced encoding (disambiguated) ──────────────────────────────
    if kbd_flags != 0 && any_mod {
        match key {
            Key::Character(s) => {
                // Shift alone on a printable key is *consumed* into the shifted
                // glyph (Shift+a → "A"), so it must be sent as literal text — only
                // Ctrl/Alt/Super need CSI-u disambiguation. Encoding shift-only as
                // `base-codepoint;shift u` made the receiving app insert the
                // unshifted (lowercase) character — i.e. no capital letters.
                if ctrl || alt || sup {
                    if let Some(c) = s.chars().next() {
                        // Base (unshifted) codepoint; modifiers in the field.
                        let cp = c.to_ascii_lowercase() as u32;
                        return Some(format!("\x1b[{cp};{modifier}u").into_bytes());
                    }
                }
                // Shift-only (or no recognised char): fall through to legacy,
                // which emits the actual shifted text bytes.
            }
            Key::Named(named) => {
                if let Some(cp) = kitty_functional_codepoint(named) {
                    return Some(format!("\x1b[{cp};{modifier}u").into_bytes());
                }
                if let Some(bytes) = modified_named_legacy(named, modifier) {
                    return Some(bytes);
                }
            }
            _ => {}
        }
    }

    // ── Legacy encoding ──────────────────────────────────────────────────────
    match key {
        Key::Character(s) => {
            let c = s.chars().next()?;
            // Ctrl(+Alt)+<char> → C0 control byte (Alt adds an ESC prefix).
            if ctrl {
                if let Some(b) = ctrl_byte(c) {
                    return Some(if alt { vec![0x1b, b] } else { vec![b] });
                }
            }
            // Alt+<char> → ESC-prefixed text (meta).
            if alt {
                let mut out = vec![0x1b];
                out.extend_from_slice(s.as_str().as_bytes());
                return Some(out);
            }
            Some(s.as_str().as_bytes().to_vec())
        }
        Key::Named(named) => {
            // Alt+Enter / Alt+Tab-style meta on named keys: ESC-prefix the base.
            if any_mod {
                if let Some(bytes) = modified_named_legacy(named, modifier) {
                    return Some(bytes);
                }
                if alt {
                    if let Some(mut base) = key_to_pty_bytes(key) {
                        let mut out = vec![0x1b];
                        out.append(&mut base);
                        return Some(out);
                    }
                }
            }
            key_to_pty_bytes(key)
        }
        _ => None,
    }
}
