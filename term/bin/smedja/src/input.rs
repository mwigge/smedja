//! Keyboard and mouse encoding for the PTY.
//!
//! Pure functions translating winit key/mouse events into the byte sequences
//! written to the PTY: legacy control/meta encoding, the kitty keyboard
//! protocol's disambiguated CSI-u form, and SGR/X10 mouse reporting.

use winit::keyboard::{Key, NamedKey};

// ── PTY key dispatch ──────────────────────────────────────────────────────────

/// Maps a winit logical key to the byte sequence to write to the PTY.
///
/// Returns `None` for keys that have no PTY representation (modifier-only keys,
/// unhandled media keys, etc.).  The caller is responsible for writing the
/// returned bytes to the PTY's stdin.
#[must_use]
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

/// Kitty keyboard-protocol modifier parameter: `1 + bitfield`, where the
/// bitfield is `shift=1, alt=2, ctrl=4, super=8`.
#[allow(clippy::fn_params_excessive_bools)]
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

/// Functional-key codepoints used by the kitty keyboard protocol's `CSI cp ; mod u`
/// encoding for the keys we disambiguate. Arrows/navigation use the legacy
/// `CSI 1 ; mod <final>` form instead and are not listed here.
#[allow(clippy::trivially_copy_pass_by_ref)]
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

/// Maps a Ctrl+<char> chord to its C0 control byte (legacy encoding).
///
/// Covers the standard `^A..^Z` letters plus the `@ [ \ ] ^ _ ?` punctuation
/// control codes. Returns `None` for characters with no control mapping.
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

/// Legacy escape sequence for a modified navigation/named key using the xterm
/// `CSI 1 ; <mod> <final>` (cursor) or `CSI <n> ; <mod> ~` (tilde) form.
/// Returns `None` for keys without a navigation encoding.
#[allow(clippy::trivially_copy_pass_by_ref)]
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

/// Encodes a key press (with modifiers) into the bytes written to the PTY.
///
/// Two regimes:
/// - **Kitty mode** (`kbd_flags != 0`): a modified key whose base has a known
///   codepoint is emitted as `CSI <cp> ; <mod> u`, so the application can tell
///   Shift+Enter from Enter, Ctrl+G from `g`, and so on. Modified navigation
///   keys use the legacy `CSI 1 ; mod <final>` form. Unmodified keys fall
///   through to the legacy base encoding.
/// - **Legacy mode** (`kbd_flags == 0`): Ctrl+<char> becomes a C0 control byte,
///   Alt+<char> is ESC-prefixed, modified navigation keys use the xterm
///   `CSI 1 ; mod` form, and everything else is the unmodified base encoding.
#[allow(clippy::fn_params_excessive_bools)]
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

// ── Mouse encoding ────────────────────────────────────────────────────────────

/// Encodes a mouse event as an SGR sequence (`\x1b[<Cb;Px;PyM` / `m`).
///
/// SGR mode (`?1006h`) uses decimal coordinates and is unambiguous for large
/// terminals.  `col` and `row` are 0-based; the escape sequence uses 1-based.
pub(crate) fn encode_mouse_sgr(col: u16, row: u16, button: u8, pressed: bool) -> Vec<u8> {
    let suffix = if pressed { b'M' } else { b'm' };
    format!("\x1b[<{};{};{}{}", button, col + 1, row + 1, suffix as char).into_bytes()
}

/// Encodes a mouse event as an X10 sequence (`\x1b[M` + 3 bytes).
///
/// Coordinates are 1-based and clamped to 223 (X10 limit).
pub(crate) fn encode_mouse_x10(col: u16, row: u16, button: u8) -> Vec<u8> {
    let cb = button.saturating_add(32);
    // Clamp in u16 space first to avoid silent truncation when col/row >= 255.
    let cx = (col + 1).min(223) as u8 + 32;
    let cy = (row + 1).min(223) as u8 + 32;
    vec![b'\x1b', b'[', b'M', cb, cx, cy]
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[test]
    fn space_key_produces_space_byte() {
        use super::key_to_pty_bytes;
        use winit::keyboard::{Key, NamedKey};
        let bytes = key_to_pty_bytes(&Key::Named(NamedKey::Space));
        assert_eq!(bytes, Some(b" ".to_vec()), "space must produce 0x20");
    }

    #[test]
    fn named_keys_produce_correct_escape_sequences() {
        use super::key_to_pty_bytes;
        use winit::keyboard::{Key, NamedKey};
        assert_eq!(
            key_to_pty_bytes(&Key::Named(NamedKey::Enter)),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            key_to_pty_bytes(&Key::Named(NamedKey::Backspace)),
            Some(b"\x7f".to_vec())
        );
        assert_eq!(
            key_to_pty_bytes(&Key::Named(NamedKey::Tab)),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            key_to_pty_bytes(&Key::Named(NamedKey::ArrowUp)),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_to_pty_bytes(&Key::Named(NamedKey::ArrowDown)),
            Some(b"\x1b[B".to_vec())
        );
        assert_eq!(
            key_to_pty_bytes(&Key::Named(NamedKey::Delete)),
            Some(b"\x1b[3~".to_vec())
        );
    }

    #[test]
    fn character_key_produces_utf8_bytes() {
        use super::key_to_pty_bytes;
        use winit::keyboard::{Key, SmolStr};
        let bytes = key_to_pty_bytes(&Key::Character(SmolStr::new("a")));
        assert_eq!(bytes, Some(b"a".to_vec()));
    }

    #[test]
    fn unknown_key_produces_none() {
        use super::key_to_pty_bytes;
        use winit::keyboard::{Key, NamedKey};
        assert_eq!(key_to_pty_bytes(&Key::Named(NamedKey::F1)), None);
    }

    // ── encode_key: legacy control / Alt / modifier handling ─────────────────

    #[test]
    fn ctrl_letter_produces_c0_control_byte_in_legacy_mode() {
        use super::encode_key;
        use winit::keyboard::{Key, SmolStr};
        // Ctrl+C → 0x03, Ctrl+G → 0x07 — the bug that broke all control keys.
        let c = encode_key(
            &Key::Character(SmolStr::new("c")),
            false,
            false,
            true,
            false,
            0,
        );
        assert_eq!(c, Some(vec![0x03]));
        let g = encode_key(
            &Key::Character(SmolStr::new("g")),
            false,
            false,
            true,
            false,
            0,
        );
        assert_eq!(g, Some(vec![0x07]));
    }

    #[test]
    fn alt_char_is_esc_prefixed_in_legacy_mode() {
        use super::encode_key;
        use winit::keyboard::{Key, SmolStr};
        let b = encode_key(
            &Key::Character(SmolStr::new("b")),
            false,
            true,
            false,
            false,
            0,
        );
        assert_eq!(b, Some(vec![0x1b, b'b']));
    }

    #[test]
    fn plain_char_is_literal_in_legacy_mode() {
        use super::encode_key;
        use winit::keyboard::{Key, SmolStr};
        let a = encode_key(
            &Key::Character(SmolStr::new("a")),
            false,
            false,
            false,
            false,
            0,
        );
        assert_eq!(a, Some(b"a".to_vec()));
    }

    #[test]
    fn modified_arrow_uses_xterm_csi_form() {
        use super::encode_key;
        use winit::keyboard::{Key, NamedKey};
        // Ctrl+ArrowRight → CSI 1 ; 5 C
        let r = encode_key(
            &Key::Named(NamedKey::ArrowRight),
            false,
            false,
            true,
            false,
            0,
        );
        assert_eq!(r, Some(b"\x1b[1;5C".to_vec()));
    }

    // ── encode_key: kitty enhanced (disambiguated) encoding ──────────────────

    #[test]
    fn shift_enter_is_csi_u_in_kitty_mode() {
        use super::encode_key;
        use winit::keyboard::{Key, NamedKey};
        // Shift+Enter with flags active → CSI 13 ; 2 u (Enter=13, shift mod=2).
        let bytes = encode_key(&Key::Named(NamedKey::Enter), true, false, false, false, 1);
        assert_eq!(bytes, Some(b"\x1b[13;2u".to_vec()));
    }

    #[test]
    fn ctrl_g_is_csi_u_in_kitty_mode() {
        use super::encode_key;
        use winit::keyboard::{Key, SmolStr};
        // Ctrl+G with flags active → CSI 103 ; 5 u (g=103, ctrl mod=5).
        let bytes = encode_key(
            &Key::Character(SmolStr::new("g")),
            false,
            false,
            true,
            false,
            1,
        );
        assert_eq!(bytes, Some(b"\x1b[103;5u".to_vec()));
    }

    #[test]
    fn unmodified_enter_is_plain_cr_even_in_kitty_mode() {
        use super::encode_key;
        use winit::keyboard::{Key, NamedKey};
        // No modifiers → falls through to the legacy base encoding.
        let bytes = encode_key(&Key::Named(NamedKey::Enter), false, false, false, false, 1);
        assert_eq!(bytes, Some(b"\r".to_vec()));
    }

    #[test]
    fn shift_letter_sends_uppercase_text_in_kitty_mode() {
        use super::encode_key;
        use winit::keyboard::{Key, SmolStr};
        // Shift+A in kitty mode must send the literal "A", not `CSI 97;2 u`,
        // otherwise the receiving app inserts a lowercase 'a' (no capitals).
        let bytes = encode_key(
            &Key::Character(SmolStr::new("A")),
            true,
            false,
            false,
            false,
            1,
        );
        assert_eq!(bytes, Some(b"A".to_vec()));
    }

    #[test]
    fn ctrl_shift_letter_still_uses_csi_u_in_kitty_mode() {
        use super::encode_key;
        use winit::keyboard::{Key, SmolStr};
        // Ctrl+Shift+A → CSI 97 ; 6 u (a=97, ctrl+shift mod = 1+1+4 = 6).
        let bytes = encode_key(
            &Key::Character(SmolStr::new("A")),
            true,
            false,
            true,
            false,
            1,
        );
        assert_eq!(bytes, Some(b"\x1b[97;6u".to_vec()));
    }

    /// Conformance table for `encode_key` covering the regression surface
    /// (caps, control keys, meta, modified Enter) in both legacy and kitty
    /// modes. New input bugs of these shapes should fail here first.
    #[test]
    #[allow(clippy::too_many_lines, clippy::type_complexity)]
    fn encode_key_conformance_table() {
        use super::encode_key;
        use winit::keyboard::{Key, NamedKey, SmolStr};
        let chr = |s: &str| Key::Character(SmolStr::new(s));
        let enter = || Key::Named(NamedKey::Enter);

        // (desc, key, shift, alt, ctrl, sup, kbd_flags, expected)
        let cases: Vec<(&str, Key, bool, bool, bool, bool, u8, &[u8])> = vec![
            (
                "plain a / legacy",
                chr("a"),
                false,
                false,
                false,
                false,
                0,
                b"a".as_slice(),
            ),
            (
                "plain a / kitty",
                chr("a"),
                false,
                false,
                false,
                false,
                1,
                b"a",
            ),
            // Caps: Shift+letter must send the uppercase glyph, not base+shift.
            (
                "Shift+A / legacy",
                chr("A"),
                true,
                false,
                false,
                false,
                0,
                b"A",
            ),
            (
                "Shift+A / kitty",
                chr("A"),
                true,
                false,
                false,
                false,
                1,
                b"A",
            ),
            // Control keys → C0 / CSI-u.
            (
                "Ctrl+C / legacy",
                chr("c"),
                false,
                false,
                true,
                false,
                0,
                b"\x03",
            ),
            (
                "Ctrl+G / kitty",
                chr("g"),
                false,
                false,
                true,
                false,
                1,
                b"\x1b[103;5u",
            ),
            (
                "Ctrl+Shift+A / kitty",
                chr("A"),
                true,
                false,
                true,
                false,
                1,
                b"\x1b[97;6u",
            ),
            // Meta.
            (
                "Alt+b / legacy",
                chr("b"),
                false,
                true,
                false,
                false,
                0,
                b"\x1bb",
            ),
            // Enter variants.
            (
                "Enter / legacy",
                enter(),
                false,
                false,
                false,
                false,
                0,
                b"\r",
            ),
            (
                "Enter / kitty no-mods",
                enter(),
                false,
                false,
                false,
                false,
                1,
                b"\r",
            ),
            (
                "Shift+Enter / kitty",
                enter(),
                true,
                false,
                false,
                false,
                1,
                b"\x1b[13;2u",
            ),
        ];

        for (desc, key, shift, alt, ctrl, sup, flags, expected) in cases {
            let got = encode_key(&key, shift, alt, ctrl, sup, flags);
            assert_eq!(
                got.as_deref(),
                Some(expected),
                "{desc}: got {got:?}, want {expected:?}"
            );
        }
    }
}
