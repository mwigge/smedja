use super::*;

/// Renders `panel` into a fresh 20×12 (inner 18×10) test terminal and returns
/// the top logical line drawn — the anchor a user actually sees at the top.
fn draw_and_top(panel: &mut MainPanel) -> usize {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut term = Terminal::new(TestBackend::new(20, 12)).unwrap();
    term.draw(|f| panel.render(f.area(), f, None, None, false))
        .unwrap();
    panel.row_logical[0]
}

// Regression: a single scroll-up from the followed bottom must move the view up
// by one logical line immediately. Previously `scroll` stayed anchored to the
// last line, whose visual start is already past `max_off`, so the window clamped
// to the bottom and the first ~viewport-height presses did nothing ("can't
// scroll back").
#[test]
fn scroll_up_from_bottom_moves_view_immediately() {
    let mut panel = MainPanel::new();
    for i in 0..100u32 {
        panel.push_line(format!("line {i}"));
    }
    let top_follow = draw_and_top(&mut panel);
    panel.scroll_up();
    let top_after = draw_and_top(&mut panel);
    assert!(
        top_after < top_follow,
        "one scroll_up must move the top line up: {top_follow} -> {top_after}"
    );
    assert_eq!(top_after, top_follow - 1, "moves exactly one logical line");
    assert!(!panel.follow, "scrolling up detaches follow");
}

// Scrolling back down to the bottom must clamp the window to the last line and
// re-arm follow so new content tracks again.
#[test]
fn scrolling_back_to_bottom_rearms_follow() {
    let mut panel = MainPanel::new();
    for i in 0..100u32 {
        panel.push_line(format!("line {i}"));
    }
    let bottom_top = draw_and_top(&mut panel);
    // Scroll well up, then all the way back down.
    for _ in 0..20 {
        panel.scroll_up();
        draw_and_top(&mut panel);
    }
    assert!(!panel.follow, "still detached mid-scroll");
    for _ in 0..40 {
        panel.scroll_down();
        draw_and_top(&mut panel);
    }
    assert!(panel.follow, "reaching the bottom re-arms follow");
    assert_eq!(
        draw_and_top(&mut panel),
        bottom_top,
        "view clamps back to the same bottom window"
    );
}

// The rendered top line never runs past the last line even if `scroll` is set
// beyond the buffer (e.g. after an over-scroll or a resize) — the window
// self-corrects to the last drawable anchor.
#[test]
fn overscrolled_anchor_clamps_to_last_window() {
    let mut panel = MainPanel::new();
    for i in 0..30u32 {
        panel.push_line(format!("line {i}"));
    }
    panel.follow = false;
    panel.scroll = 999; // absurd anchor past the end
    let top = draw_and_top(&mut panel);
    // 30 lines, inner height 10 → the bottom window starts at logical line 20.
    assert_eq!(top, 20, "over-scrolled anchor clamps to the last window");
    assert!(panel.follow, "clamping to the bottom re-arms follow");
}

// L121: +foo → LineStyle::Added
#[test]
fn plus_prefix_yields_added_style() {
    let mut panel = MainPanel::new();
    panel.push_line("+foo".into());
    assert_eq!(panel.lines.len(), 1);
    assert_eq!(panel.lines[0].style, LineStyle::Added);
    assert_eq!(panel.lines[0].text, "+foo");
}

// L121: -bar → LineStyle::Removed
#[test]
fn minus_prefix_yields_removed_style() {
    let mut panel = MainPanel::new();
    panel.push_line("-bar".into());
    assert_eq!(panel.lines.len(), 1);
    assert_eq!(panel.lines[0].style, LineStyle::Removed);
}

// L121: triple-backtick then `let x = 1;` → second line is LineStyle::Code
#[test]
fn fence_then_code_line_is_code_style() {
    let mut panel = MainPanel::new();
    panel.push_line("```".into());
    panel.push_line("let x = 1;".into());
    // The fence itself is line 0 (Code), the code line is line 1 (Code).
    assert_eq!(panel.lines.len(), 2);
    assert_eq!(panel.lines[1].style, LineStyle::Code);
}

// L121: scroll=5 on 10 lines — rendered lines start at index 5
#[test]
fn scroll_offsets_visible_range() {
    let mut panel = MainPanel::new();
    for i in 0..10u32 {
        panel.push_line(format!("line {i}"));
    }
    panel.scroll = 5;
    // The first line that would be rendered (skipping scroll) is index 5.
    let visible: Vec<&StyledLine> = panel.lines.iter().skip(panel.scroll).collect();
    assert_eq!(visible.len(), 5);
    assert_eq!(visible[0].text, "line 5");
}

#[test]
fn wrap_splits_long_line_into_rows() {
    let line = Line::from("abcdefghij"); // 10 display cols
    let rows = wrap_line_to(&line, 4);
    assert_eq!(rows.len(), 3); // 4 + 4 + 2
    let joined: String = rows
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(joined, "abcdefghij");
}

#[test]
fn wrap_preserves_span_style() {
    let line = Line::from(Span::styled("abcdef", Style::default().fg(Color::Red)));
    let rows = wrap_line_to(&line, 3);
    assert_eq!(rows.len(), 2);
    for r in &rows {
        for s in &r.spans {
            assert_eq!(s.style.fg, Some(Color::Red));
        }
    }
}

#[test]
fn wrap_empty_line_is_single_row() {
    assert_eq!(wrap_line_to(&Line::from(String::new()), 10).len(), 1);
}

#[test]
fn finalize_classifies_streamed_diff_line() {
    let mut panel = MainPanel::new();
    panel.push_line(String::new()); // tail partial line
    panel.push_delta("+added line"); // streamed text, unclassified
    panel.finalize_last_line();
    assert_eq!(panel.lines.last().unwrap().style, LineStyle::Added);
    assert_eq!(panel.lines.last().unwrap().text, "+added line");
}

#[test]
fn finalize_preserves_already_styled_card() {
    let mut panel = MainPanel::new();
    panel.push_styled_line(Line::from(Span::raw("⌘ bash")));
    panel.finalize_last_line();
    let last = panel.lines.last().unwrap();
    assert_eq!(last.text, "⌘ bash");
    assert!(last.spans.is_some(), "card spans must survive finalize");
}

#[test]
fn push_styled_line_backs_text_with_flattened_spans() {
    let mut panel = MainPanel::new();
    panel.push_styled_line(Line::from(vec![Span::raw("⌘ bash"), Span::raw("  find .")]));
    let last = panel.lines.last().unwrap();
    assert_eq!(last.text, "⌘ bash  find .");
    assert!(last.spans.is_some());
}

#[test]
fn tool_result_meta_line_renders_dim_spans() {
    let mut panel = MainPanel::new();
    panel.push_line("↳ ok · 107 chars".into());
    assert!(panel.lines.last().unwrap().spans.is_some());
}

#[test]
fn is_diff_marker_recognizes_headers_not_prose() {
    assert!(is_diff_marker("@@ -1,2 +1,3 @@"));
    assert!(is_diff_marker("diff --git a/x b/x"));
    assert!(is_diff_marker("--- a/x.rs"));
    assert!(is_diff_marker("+++ /dev/null"));
    assert!(!is_diff_marker("--- a thought --- continued"));
    assert!(!is_diff_marker("hello world"));
    assert!(!is_diff_marker("+content")); // bare +/- handled elsewhere
}

#[test]
fn diff_fence_styles_hunk_and_content_with_clean_copy_text() {
    let mut panel = MainPanel::new();
    panel.push_line("```diff".into());
    panel.push_line("@@ -1,2 +1,3 @@".into());
    panel.push_line("+added".into());
    panel.push_line(" context".into());
    panel.push_line("```".into());

    let hunk = panel
        .lines
        .iter()
        .find(|l| l.text.starts_with("@@"))
        .unwrap();
    assert!(hunk.spans.is_some(), "hunk header should be styled");
    let added = panel.lines.iter().find(|l| l.text == "+added").unwrap();
    assert!(
        added.spans.is_some(),
        "added line should carry gutter spans"
    );
    // Gutter is display-only — the backing text stays clean for copy/yank.
    assert_eq!(added.text, "+added");
}

#[test]
fn inline_markdown_styles_and_strips_markers() {
    // Bold/italic/code produce spans; flattened text drops the markers.
    let line = inline_markdown_spans("use **bold** and `code` here").unwrap();
    let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(flat, "use bold and code here");
    // No markup → None (cheap plain path preserved).
    assert!(inline_markdown_spans("just plain prose").is_none());
    // Flanking guard: spaced asterisks (multiplication / bullets) are not italic.
    assert!(inline_markdown_spans("a * b * c").is_none());
}

#[test]
fn inline_markdown_line_keeps_clean_copy_text() {
    let mut panel = MainPanel::new();
    panel.push_line("a **strong** word".into());
    let last = panel.lines.last().unwrap();
    assert!(last.spans.is_some());
    assert_eq!(last.text, "a strong word"); // markers stripped for copy
}

#[test]
fn table_rows_render_cells_and_delimiter_rule() {
    assert!(is_table_row("| a | b |"));
    assert!(!is_table_row("no pipes here"));
    assert!(!is_table_row("trailing pipe a |")); // no leading pipe
    assert_eq!(
        table_cells("| a | b |"),
        vec!["a".to_owned(), "b".to_owned()]
    );

    let mut panel = MainPanel::new();
    panel.push_line("| Name | Age |".into());
    panel.push_line("|------|-----|".into());
    panel.push_line("| Ann  | 30  |".into());
    // All three rendered as styled lines; copy text stays the raw markdown.
    for sl in &panel.lines {
        assert!(sl.spans.is_some());
    }
    assert_eq!(panel.lines[0].text, "| Name | Age |");
}

#[test]
fn block_markdown_styles_headings_quotes_rules_and_lists() {
    // ATX heading: marker stripped, bold accent applied.
    let h = block_markdown_spans("## Section title").unwrap();
    let flat: String = h.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(flat, "Section title");
    assert!(h
        .spans
        .iter()
        .any(|s| s.style.add_modifier.contains(Modifier::BOLD)));

    // Thematic break: a dim horizontal rule of box-drawing dashes.
    let hr = block_markdown_spans("---").unwrap();
    let rule: String = hr.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        rule.chars().all(|c| c == '\u{2500}'),
        "hr is a ─ run: {rule}"
    );
    assert!(hr.spans[0].style.add_modifier.contains(Modifier::DIM));

    // Unordered list: red-`-` bug is gone — the marker becomes a bullet.
    let li = block_markdown_spans("- do the thing").unwrap();
    let bullet: String = li.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(bullet.starts_with('\u{2022}'), "bullet: {bullet}");
    assert!(bullet.contains("do the thing"));

    // Ordered list keeps its number.
    let ol = block_markdown_spans("3. third").unwrap();
    let num: String = ol.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(num.starts_with("3. "), "ordered: {num}");

    // Blockquote: gutter bar + body.
    let q = block_markdown_spans("> quoted").unwrap();
    let qt: String = q.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        qt.contains('\u{258d}') && qt.contains("quoted"),
        "quote: {qt}"
    );

    // Plain prose is left to the cheap path.
    assert!(block_markdown_spans("just a sentence").is_none());
}

#[test]
fn push_line_renders_list_without_red_removed_style() {
    // Regression: "- item" used to classify as a red diff removal.
    let mut panel = MainPanel::new();
    panel.push_line("- a bullet".into());
    let last = panel.lines.last().unwrap();
    assert_eq!(last.style, LineStyle::Normal, "list item is not a removal");
    assert!(last.spans.is_some(), "list item is styled");
    assert!(last.text.starts_with('\u{2022}'));
}

#[test]
fn heading_is_bold_and_hr_is_dim_chrome() {
    let mut panel = MainPanel::new();
    panel.push_line("# Title".into());
    panel.push_line("***".into());
    let heading = &panel.lines[0];
    assert!(heading
        .spans
        .as_ref()
        .unwrap()
        .spans
        .iter()
        .any(|s| s.style.add_modifier.contains(Modifier::BOLD)));
    let hr = &panel.lines[1];
    assert!(hr
        .spans
        .as_ref()
        .unwrap()
        .spans
        .iter()
        .all(|s| s.style.add_modifier.contains(Modifier::DIM)));
}

// Change 5 guard: the syntax highlighter must give tokens *distinct* colours,
// not collapse to one — assert a rust sample yields >= 3 different fg styles.
#[test]
fn highlight_code_yields_at_least_three_distinct_styles() {
    let src = "fn main() {\n    // greet\n    let name = \"world\";\n    let n = 42;\n}";
    let lines = highlight_code("rust", src);
    let mut colors = std::collections::HashSet::new();
    for l in &lines {
        if let Some(spans) = &l.spans {
            for s in &spans.spans {
                if let Some(fg) = s.style.fg {
                    colors.insert(format!("{fg:?}"));
                }
            }
        }
    }
    assert!(
        colors.len() >= 3,
        "expected >= 3 distinct token colours, got {}: {colors:?}",
        colors.len()
    );
}

#[test]
fn standalone_hunk_header_is_styled() {
    let mut panel = MainPanel::new();
    panel.push_line("@@ -10,3 +10,4 @@ fn main()".into());
    assert!(panel.lines.last().unwrap().spans.is_some());
}

#[test]
fn scroll_up_detaches_follow_and_bottom_rearms() {
    let mut panel = MainPanel::new();
    for i in 0..10u32 {
        panel.push_line(format!("line {i}"));
    }
    assert!(panel.follow, "streaming default is follow");
    panel.scroll_up();
    assert!(!panel.follow, "manual scroll-up detaches follow");
    for _ in 0..20 {
        panel.scroll_down();
    }
    assert!(panel.follow, "returning to the bottom re-arms follow");
}

#[test]
fn render_wraps_long_line_and_hit_tests() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut panel = MainPanel::new();
    panel.push_line("x".repeat(20)); // one long logical line
    let mut term = Terminal::new(TestBackend::new(12, 8)).unwrap(); // inner width 10
    term.draw(|f| {
        let area = f.area();
        panel.render(area, f, None, None, false);
    })
    .unwrap();

    // 20 cols / inner-10 → 2 visual rows, both mapping to logical line 0.
    assert_eq!(panel.pos_at(1, 1).map(|(l, _)| l), Some(0));
    assert_eq!(panel.pos_at(1, 2).map(|(l, _)| l), Some(0));
    // Second visual row starts at char offset 10 within the logical line.
    assert_eq!(panel.pos_at(1, 2), Some((0, 10)));
    // First row, leftmost cell → char 0.
    assert_eq!(panel.pos_at(1, 1), Some((0, 0)));
    // The border cell (0,0) is outside the inner area.
    assert_eq!(panel.pos_at(0, 0), None);
}

#[test]
fn selection_text_extracts_partial_and_multiline() {
    let mut panel = MainPanel::new();
    panel.push_line("hello world".into()); // line 0
    panel.push_line("second line".into()); // line 1
                                           // Partial within one line: chars [0,5) of line 0 → "hello".
    assert_eq!(panel.selection_text((0, 0), (0, 5)), "hello");
    // Order-independent.
    assert_eq!(panel.selection_text((0, 5), (0, 0)), "hello");
    // Across lines: from line0 col6 to line1 col6 → "world\nsecond".
    assert_eq!(panel.selection_text((0, 6), (1, 6)), "world\nsecond");
}

// L121: plain line → LineStyle::Normal
#[test]
fn plain_line_yields_normal_style() {
    let mut panel = MainPanel::new();
    panel.push_line("hello world".into());
    assert_eq!(panel.lines[0].style, LineStyle::Normal);
}

// L121: closing fence exits code mode
#[test]
fn closing_fence_exits_code_mode() {
    let mut panel = MainPanel::new();
    panel.push_line("```".into());
    panel.push_line("code line".into());
    panel.push_line("```".into());
    // After the closing fence, a new line should be Normal.
    panel.push_line("normal again".into());
    assert_eq!(panel.lines.last().unwrap().style, LineStyle::Normal);
}

// L121: language-tagged fence populates code_lang
#[test]
fn fence_with_lang_sets_code_lang() {
    let mut panel = MainPanel::new();
    panel.push_line("```rust".into());
    assert_eq!(panel.code_lang, "rust");
    assert!(panel.in_code_block);
}

// L135-L137: apply_syntect returns at least one StyledLine with non-empty text
#[test]
fn apply_syntect_rust_returns_nonempty_lines() {
    let lines = apply_syntect("rust", "let x = 1;");
    assert!(!lines.is_empty(), "expected at least one highlighted line");
    assert!(
        lines.iter().any(|l| !l.text.is_empty()),
        "expected at least one line with non-empty text"
    );
}

// L135-L137: apply_syntect with unknown lang falls back gracefully
#[test]
fn apply_syntect_unknown_lang_falls_back() {
    let lines = apply_syntect("xyzzy_unknown", "hello world");
    assert!(!lines.is_empty());
    assert_eq!(lines[0].style, LineStyle::Code);
}

// L135-L137: syntect code lines inside a rust fence are Code style
#[test]
fn fence_rust_code_lines_use_syntect() {
    let mut panel = MainPanel::new();
    panel.push_line("```rust".into());
    panel.push_line("fn main() {}".into());
    panel.push_line("```".into());
    // The code line (index 1) must be Code style.
    assert_eq!(panel.lines[1].style, LineStyle::Code);
}

// apply_syntect: rust code gets per-character forge-coloured spans
#[test]
fn apply_syntect_rust_produces_rgb_spans() {
    let lines = apply_syntect("rust", "let x = 1;");
    // At least one line should have coloured spans from the syntect classifier.
    let has_spans = lines.iter().any(|l| l.spans.is_some());
    assert!(has_spans, "syntect should produce spans for rust code");
}

// apply_syntect must colour tokens with forge CODE_* palette slots only.
#[test]
fn apply_syntect_uses_forge_palette_colours() {
    let p = palette();
    let forge = [
        p.code_default,
        p.code_keyword,
        p.code_string,
        p.code_number,
        p.code_comment,
        p.code_type,
        p.code_macro,
    ];
    let lines = apply_syntect("rust", "// c\nlet s = \"hi\";");
    let mut saw_colored = false;
    for l in &lines {
        if let Some(spans) = &l.spans {
            for span in &spans.spans {
                if let Some(fg) = span.style.fg {
                    saw_colored = true;
                    assert!(
                        forge.contains(&fg),
                        "span colour {fg:?} must be a forge CODE_* slot"
                    );
                }
            }
        }
    }
    assert!(saw_colored, "expected at least one coloured span");
}

// highlight_code: tree-sitter path covers rust / go / python / typescript.
#[test]
fn highlight_code_treesitter_rust() {
    let lines = highlight_code("rust", "fn main() {\n    let x = 42;\n}");
    assert_eq!(lines.len(), 3, "line count preserved");
    assert!(lines.iter().all(|l| l.spans.is_some()), "annotated spans");
}

#[test]
fn highlight_code_treesitter_go() {
    let src = "package main\n\nfunc main() {\n\ts := \"hi\"\n}";
    let lines = highlight_code("go", src);
    assert_eq!(lines.len(), 5);
    assert!(lines.iter().any(|l| l.spans.is_some()));
}

#[test]
fn highlight_code_treesitter_python() {
    let src = "def greet(n):\n    # hi\n    return \"x\"";
    let lines = highlight_code("python", src);
    assert_eq!(lines.len(), 3);
    assert!(lines.iter().any(|l| l.spans.is_some()));
}

#[test]
fn highlight_code_treesitter_typescript() {
    let src = "const x: number = 1;\nfunction f() { return x; }";
    let lines = highlight_code("ts", src);
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().any(|l| l.spans.is_some()));
}

#[test]
fn highlight_code_unknown_lang_falls_back_without_panic() {
    let lines = highlight_code("xyzzy_unknown", "hello world\nfoo bar");
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].style, LineStyle::Code);
}

#[test]
fn ts_lang_maps_aliases() {
    assert_eq!(ts_lang("rs"), Some(TsLang::Rust));
    assert_eq!(ts_lang("golang"), Some(TsLang::Go));
    assert_eq!(ts_lang("py"), Some(TsLang::Python));
    assert_eq!(ts_lang("typescript"), Some(TsLang::TypeScript));
    assert_eq!(ts_lang("cobol"), None);
}

// push_delta: appending two deltas to an empty panel produces one line
#[test]
fn push_delta_appends_to_in_progress_line() {
    let mut panel = MainPanel::new();
    panel.push_delta("hello");
    panel.push_delta(" world");
    // Should produce one line with "hello world"
    let content = panel.visible_text();
    assert!(
        content.contains("hello world"),
        "push_delta should append to same line"
    );
}

// push_delta: first call on an empty panel creates a new line
#[test]
fn push_delta_creates_new_line_when_empty() {
    let mut panel = MainPanel::new();
    panel.push_delta("first");
    let content = panel.visible_text();
    assert!(content.contains("first"));
}

#[test]
fn scroll_down_clamps_at_last_line() {
    let mut panel = MainPanel::new();
    for i in 0..5u32 {
        panel.push_line(format!("line {i}"));
    }
    panel.scroll_to_bottom();
    let before = panel.scroll;
    panel.scroll_down();
    assert_eq!(
        panel.scroll, before,
        "scroll must not exceed last line index"
    );
}

#[test]
fn scroll_up_clamps_at_zero() {
    let mut panel = MainPanel::new();
    panel.push_line("only".into());
    panel.scroll_up();
    assert_eq!(panel.scroll, 0);
}

#[test]
fn scroll_to_top_sets_display_start() {
    let mut panel = MainPanel::new();
    for i in 0..5u32 {
        panel.push_line(format!("line {i}"));
    }
    panel.scroll = 4;
    panel.scroll_to_top();
    assert_eq!(panel.scroll, panel.display_start);
}

#[test]
fn clear_display_advances_watermark_and_scroll() {
    let mut panel = MainPanel::new();
    for i in 0..5u32 {
        panel.push_line(format!("line {i}"));
    }
    panel.clear_display();
    assert_eq!(panel.display_start, 5);
    assert_eq!(panel.scroll, 5);
}

#[test]
fn scroll_up_does_not_cross_display_start() {
    let mut panel = MainPanel::new();
    for i in 0..5u32 {
        panel.push_line(format!("line {i}"));
    }
    panel.clear_display();
    panel.push_line("after clear".into());
    panel.scroll_up();
    assert_eq!(
        panel.scroll, panel.display_start,
        "scroll must not cross the clear watermark"
    );
}

#[test]
fn new_lines_after_clear_are_rendered() {
    let mut panel = MainPanel::new();
    for i in 0..3u32 {
        panel.push_line(format!("old {i}"));
    }
    panel.clear_display();
    panel.push_line("new line".into());
    // scroll == display_start == 3; new line is at index 3 → visible
    let vis: Vec<&StyledLine> = panel
        .lines
        .iter()
        .skip(panel.scroll.max(panel.display_start))
        .collect();
    assert_eq!(vis.len(), 1);
    assert_eq!(vis[0].text, "new line");
}

#[test]
fn scroll_to_bottom_sets_last_index() {
    let mut panel = MainPanel::new();
    for i in 0..5u32 {
        panel.push_line(format!("line {i}"));
    }
    panel.scroll_to_bottom();
    assert_eq!(panel.scroll, 4);
}

#[test]
fn lines_text_returns_inclusive_range() {
    let mut panel = MainPanel::new();
    for i in 0..5u32 {
        panel.push_line(format!("L{i}"));
    }
    let got = panel.lines_text(1, 3);
    assert_eq!(got, vec!["L1", "L2", "L3"]);
}

#[test]
fn lines_text_handles_reversed_order() {
    let mut panel = MainPanel::new();
    for i in 0..5u32 {
        panel.push_line(format!("L{i}"));
    }
    let got = panel.lines_text(3, 1);
    assert_eq!(got, vec!["L1", "L2", "L3"]);
}

#[test]
fn push_line_auto_scrolls_when_at_bottom() {
    let mut panel = MainPanel::new();
    // Push 5 lines — each should auto-scroll since we start at bottom.
    for i in 0..5u32 {
        panel.push_line(format!("line {i}"));
    }
    assert_eq!(
        panel.scroll, 4,
        "scroll should follow new lines when at bottom"
    );
}

#[test]
fn push_line_does_not_auto_scroll_when_scrolled_up() {
    let mut panel = MainPanel::new();
    for i in 0..5u32 {
        panel.push_line(format!("line {i}"));
    }
    panel.scroll_up(); // user scrolls up → detaches bottom-follow
    let before = panel.scroll;
    panel.push_line("new line".into());
    assert_eq!(
        panel.scroll, before,
        "scroll must stay when user has scrolled up"
    );
}

#[test]
fn clamp_scroll_reduces_out_of_bounds_scroll() {
    let mut panel = MainPanel::new();
    for i in 0..3u32 {
        panel.push_line(format!("line {i}"));
    }
    panel.scroll = 100;
    panel.clamp_scroll();
    assert_eq!(panel.scroll, 2);
}

// --- math rendering -------------------------------------------------------

#[test]
fn render_math_converts_inline_greek() {
    let out = render_math("$\\alpha + \\beta$");
    assert_eq!(
        out, "α + β",
        "Greek letters must be converted; got: {out:?}"
    );
}

#[test]
fn render_math_converts_superscript_digit() {
    let out = render_math("$E = mc^2$");
    assert_eq!(out, "E = mc²", "^2 must become ²; got: {out:?}");
}

#[test]
fn render_math_converts_subscript_digit() {
    let out = render_math("$x_0$");
    assert_eq!(out, "x₀", "subscript 0 must become ₀; got: {out:?}");
}

#[test]
fn render_math_leaves_unknown_commands_verbatim() {
    let out = render_math("$\\unknowncmd$");
    assert!(
        out.contains("\\unknowncmd"),
        "unrecognised commands must be passed through; got: {out:?}"
    );
}

#[test]
fn render_math_does_not_touch_text_outside_dollars() {
    let out = render_math("no math here");
    assert_eq!(out, "no math here");
}

#[test]
fn render_math_dollar_sign_without_closing_delimiter_is_unchanged() {
    let out = render_math("cost is $5 per month");
    // "5 per month" has no closing $, so the $ should be preserved
    assert!(
        out.contains('$') || out.contains("5 per month"),
        "unclosed $ must not panic; got: {out:?}"
    );
}

#[test]
fn push_line_applies_math_rendering() {
    let mut panel = MainPanel::new();
    panel.push_line("$\\pi$ is about 3.14".to_owned());
    assert!(
        panel.lines[0].text.contains('π'),
        "push_line must apply math rendering; got: {:?}",
        panel.lines[0].text
    );
}

// Helper: true when every non-empty span on `sl` carries the DIM modifier —
// the "chrome recedes" invariant for a transcript line.
fn line_is_dim(sl: &StyledLine) -> bool {
    sl.spans.as_ref().is_some_and(|l| {
        l.spans
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::DIM))
    })
}

// The reframed transcript fix: an EXTERNAL runner's (codex/claude) answer body
// gains markdown hierarchy — headings bold, bullets aligned, links styled —
// while every surrounding chrome line (execute banner, `↳ ok` echo, clipboard
// notice, trace footer) recedes to dim so the content reads brighter.
#[test]
fn external_runner_body_gets_hierarchy_and_chrome_is_dim() {
    // --- Content gains hierarchy ---------------------------------------
    // codex prints its own bullet glyph "• …"; it must align + style, not
    // read as flat prose.
    let bullet = block_markdown_spans("\u{2022} High: something").expect("• is a list item");
    let flat: String = bullet.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        flat.starts_with('\u{2022}'),
        "• bullet stays a bullet: {flat}"
    );
    assert!(flat.contains("High: something"));

    // A markdown heading in the answer body reads bold.
    let heading = block_markdown_spans("## Findings").expect("heading");
    assert!(heading
        .spans
        .iter()
        .any(|s| s.style.add_modifier.contains(Modifier::BOLD)));

    // A link reads as an accent-underlined anchor with the url dimmed.
    let link = inline_markdown_spans("see [orchestrator/mod.rs](path/to#L1) now").expect("link");
    assert!(
        link.spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::UNDERLINED)),
        "link label is underlined"
    );

    // --- Chrome recedes to dim -----------------------------------------
    let mut panel = MainPanel::new();
    panel.push_line(
        "\u{23f5} execute \u{00b7} /usr/bin/bash -lc 'git status' \u{00b7} \u{2713} 0ms".into(),
    );
    panel.push_line("\u{21b3} ok \u{00b7} [git status]".into());
    panel.push_line("\u{2713} 63 lines copied to clipboard".into());
    panel.push_line(
        "\u{21b3} 345635\u{2191} 3729\u{2193} \u{00b7} trace: 00-000 \u{00b7} traces not exported"
            .into(),
    );
    for (i, sl) in panel.lines.iter().enumerate() {
        assert!(
            line_is_dim(sl),
            "chrome line {i} must be dim: {:?}",
            sl.text
        );
    }
    // is_dim_chrome classifies each of those, and never plain content.
    assert!(is_dim_chrome(
        "\u{23f5} execute \u{00b7} cmd \u{00b7} \u{2713} 0ms"
    ));
    assert!(is_dim_chrome("\u{2717} 2 lines copied to clipboard"));
    assert!(!is_dim_chrome("Findings"));
    assert!(!is_dim_chrome("\u{2022} High: real content"));
}
