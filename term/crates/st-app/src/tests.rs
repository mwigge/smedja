use super::{tier_badge_text, App};

#[test]
fn tier_badge_resolves_to_pua_glyph_when_apc_supported() {
    let mut reg = st_glyph::GlyphRegistry::new();
    st_glyph::register_builtin_glyphs(&mut reg);
    let badge = tier_badge_text(&reg, "deep", "xterm-wezterm");
    let cp = reg.lookup("smedja.tier.deep").expect("deep is registered");
    assert_eq!(badge, cp.to_string());
}

#[test]
fn tier_badge_falls_back_to_text_when_apc_unsupported() {
    let mut reg = st_glyph::GlyphRegistry::new();
    st_glyph::register_builtin_glyphs(&mut reg);
    let badge = tier_badge_text(&reg, "deep", "xterm-256color");
    assert_eq!(badge, "[deep]");
}

#[test]
fn tier_badge_unknown_tier_is_unchanged() {
    let reg = st_glyph::GlyphRegistry::new();
    assert_eq!(tier_badge_text(&reg, "cloud", "xterm-wezterm"), "[cloud]");
}

fn make_app() -> App {
    let config = st_config::Config::default();
    App::new(config, "/bin/sh".to_owned(), Vec::new())
}

#[test]
fn app_initialises_with_empty_windows() {
    let app = make_app();
    assert!(app.windows.is_empty());
}

#[test]
fn app_initialises_with_one_tab() {
    let app = make_app();
    assert_eq!(app.tab_bar.tabs.len(), 1);
    assert_eq!(app.split_layouts.len(), 1);
}

#[test]
fn build_window_title_contains_required_parts() {
    use super::build_window_title;
    let title = build_window_title(
        Some("fast"),
        Some("impl"),
        Some("abc12345xyz"),
        Some("/home/u/proj"),
    );
    assert!(title.contains("smedja"), "must contain app name");
    assert!(title.contains("[fast]"), "must contain tier");
    assert!(title.contains("[impl]"), "must contain mode");
    assert!(
        title.contains("abc12345"),
        "must contain first 8 chars of session_id"
    );
    assert!(title.contains("/home/u/proj"), "must contain cwd");
}

#[test]
fn build_window_title_all_none_returns_smedja() {
    use super::build_window_title;
    let title = build_window_title(None, None, None, None);
    assert_eq!(title, "smedja");
}

#[test]
fn truncate_cwd_long_path_starts_with_ellipsis() {
    use super::truncate_cwd;
    let long = "/very/long/path/that/exceeds/limit";
    let result = truncate_cwd(long, 10);
    assert!(
        result.starts_with('\u{2026}'),
        "expected … prefix, got '{result}'"
    );
    assert!(
        result.chars().count() <= 11,
        "result must be at most max+1 chars"
    );
}

#[test]
fn truncate_cwd_short_path_unchanged() {
    use super::truncate_cwd;
    let short = "/short";
    let result = truncate_cwd(short, 40);
    assert_eq!(result, "/short");
}

#[test]
fn truncate_cwd_multibyte_does_not_panic_on_boundary() {
    use super::truncate_cwd;
    // A path whose tail boundary falls inside multibyte chars: a raw byte
    // slice would panic here. Each `é` is 2 bytes, `ä` 2, `语` 3.
    let path = "/home/josé/projets/café/编程语言/source";
    let result = truncate_cwd(path, 10);
    assert!(result.starts_with('\u{2026}'));
    assert_eq!(result.chars().count(), 11, "ellipsis + last 10 chars");
}

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
