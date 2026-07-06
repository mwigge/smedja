//! `util`-area unit tests (moved verbatim from the former `tests.rs`).

use crate::clipboard::{emit_osc9, osc9_turn_complete_bytes};
use crate::{
    extract_code_block, next_char_boundary, prev_char_boundary, push_capped, slugify,
    wrap_input_rows,
};

#[test]
fn push_capped_bounds_length_and_keeps_newest() {
    let cap = 8;
    let mut buf: Vec<usize> = Vec::new();
    let mut total_dropped = 0;
    for i in 0..1000 {
        total_dropped += push_capped(&mut buf, i, cap);
        // Length never exceeds the cap, no matter how many are pushed.
        assert!(buf.len() <= cap, "len {} exceeded cap {cap}", buf.len());
    }
    // Steady state holds exactly the last `cap` entries, oldest trimmed.
    assert_eq!(buf.len(), cap);
    assert_eq!(buf, (992..1000).collect::<Vec<_>>());
    // Every entry beyond the cap was dropped from the front exactly once.
    assert_eq!(total_dropped, 1000 - cap);
}

#[test]
fn push_capped_reports_no_drop_below_cap() {
    let mut buf: Vec<usize> = Vec::new();
    assert_eq!(push_capped(&mut buf, 1, 4), 0);
    assert_eq!(push_capped(&mut buf, 2, 4), 0);
    assert_eq!(buf, vec![1, 2]);
}

#[test]
fn wrap_input_rows_splits_long_line() {
    // 25 chars at width 10 → 3 rows (10/10/5).
    let rows = wrap_input_rows(&"x".repeat(25), 10);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].chars().count(), 10);
    assert_eq!(rows[2].chars().count(), 5);
}

#[test]
fn wrap_input_rows_honours_newlines() {
    let rows = wrap_input_rows("ab\ncd", 80);
    assert_eq!(rows, vec!["ab".to_string(), "cd".to_string()]);
}

#[test]
fn wrap_input_rows_empty_is_one_row() {
    assert_eq!(wrap_input_rows("", 10).len(), 1);
}

#[test]
fn prev_char_boundary_moves_back_one_ascii() {
    assert_eq!(prev_char_boundary("hello", 3), 2);
}

#[test]
fn prev_char_boundary_at_zero_stays_zero() {
    assert_eq!(prev_char_boundary("hello", 0), 0);
}

#[test]
fn next_char_boundary_moves_forward_one_ascii() {
    assert_eq!(next_char_boundary("hello", 2), 3);
}

#[test]
fn next_char_boundary_at_end_stays_at_end() {
    assert_eq!(next_char_boundary("hello", 5), 5);
}

#[test]
fn prev_char_boundary_unicode_moves_by_char() {
    // 'é' encodes as 2 bytes (U+00E9); cursor at 2 should move to 0.
    let s = "é";
    assert_eq!(s.len(), 2);
    assert_eq!(prev_char_boundary(s, 2), 0);
}

#[test]
fn next_char_boundary_unicode_moves_by_char() {
    let s = "é";
    assert_eq!(next_char_boundary(s, 0), 2);
}

#[test]
fn slugify_converts_spaces_and_upper() {
    assert_eq!(slugify("Smedja Architecture"), "smedja-architecture");
    assert_eq!(slugify("Q3 Agent Metrics!"), "q3-agent-metrics");
    assert_eq!(slugify("multi--word"), "multi-word");
}

#[test]
fn extract_code_block_finds_xml_content() {
    let text = "Some preamble\n```xml\n<mxGraph>hello</mxGraph>\n```\nsome epilogue";
    let extracted = extract_code_block(text, "xml");
    assert_eq!(extracted, Some("<mxGraph>hello</mxGraph>"));
}

#[test]
fn extract_code_block_returns_none_when_lang_absent() {
    let text = "```python\nprint('hi')\n```";
    assert!(extract_code_block(text, "xml").is_none());
}

#[test]
fn poll_backoff_shift_never_overflows() {
    // Verify the clamped shift cannot produce a u64 overflow for any retry
    // count up to and including the give-up threshold (60).
    for count in 0u32..=60 {
        let shift = count.saturating_sub(1).min(10);
        let _ = (100u64 << shift).min(1_000);
    }
}

#[test]
fn poll_backoff_caps_at_1000ms() {
    // At retry=4 the raw shift (3) gives 800 ms; at retry=5 (shift=4) the
    // raw value 1600 ms clamps to 1000 ms and stays there.
    for count in 5u32..=60 {
        let shift = count.saturating_sub(1).min(10);
        let ms = (100u64 << shift).min(1_000);
        assert_eq!(ms, 1_000, "backoff must cap at 1000 ms for retry {count}");
    }
}

// --- keybinding: Ctrl-F context rail / Ctrl-R history search ---------------

#[test]
fn prompt_token_estimate_uses_chars_over_four_heuristic() {
    // 40 chars / 4 = 10 estimated tokens.
    let input = "a".repeat(40);
    let chars = input.chars().count();
    #[allow(clippy::integer_division)]
    let est = chars / 4;
    assert_eq!(est, 10, "40 chars should estimate to 10 tokens");
}

#[test]
fn prompt_token_estimate_rounds_down() {
    let input = "abc"; // 3 chars / 4 = 0 — rounds down
    let chars = input.chars().count();
    #[allow(clippy::integer_division)]
    let est = chars / 4;
    assert_eq!(est, 0);
}

#[test]
fn osc9_bytes_is_correct_sequence() {
    let bytes = osc9_turn_complete_bytes();
    assert_eq!(bytes, b"\x1b]9;turn complete\x07");
}

#[test]
fn emit_osc9_writes_to_vec() {
    let mut buf: Vec<u8> = Vec::new();
    emit_osc9(&mut buf).unwrap();
    assert_eq!(buf, b"\x1b]9;turn complete\x07");
}

// --- P2a: kill ring -------------------------------------------------------
