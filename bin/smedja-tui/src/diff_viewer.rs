//! Diff viewer — unified and side-by-side rendering for diff overlays.

use crate::main_panel::highlight_code;
use crate::theme::palette;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// Detects whether `lines` look like unified diff output.
///
/// Requires at least one `@@` hunk header and at least one `+` or `-` line.
#[must_use]
pub fn is_diff_content(lines: &[String]) -> bool {
    let has_hunk = lines.iter().any(|l| l.starts_with("@@"));
    let has_change = lines
        .iter()
        .any(|l| l.starts_with('+') || l.starts_with('-'));
    has_hunk && has_change
}

/// Renders the diff as a scrollable unified view inside `rect`.
pub fn render_unified(
    lines: &[String],
    scroll: usize,
    rect: Rect,
    no_color: bool,
    frame: &mut Frame,
) {
    let visible_h = rect.height.saturating_sub(2) as usize;
    let lang = detect_diff_lang(lines).unwrap_or("");
    let rendered: Vec<Line<'static>> = lines
        .iter()
        .skip(scroll)
        .take(visible_h)
        .map(|l| {
            if !l.starts_with("+++") && l.starts_with('+') {
                highlighted_diff_line(l, true, lang, no_color)
            } else if !l.starts_with("---") && l.starts_with('-') {
                highlighted_diff_line(l, false, lang, no_color)
            } else {
                Line::from(vec![Span::raw(l.clone())])
            }
        })
        .collect();
    let widget = Paragraph::new(rendered).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" tool detail  [S] split "),
    );
    frame.render_widget(widget, rect);
}

/// Renders the diff as a side-by-side two-column view inside `rect`.
///
/// Left column shows old lines (removed); right column shows new lines (added).
/// Context lines appear in both columns.
pub fn render_split(
    lines: &[String],
    scroll: usize,
    rect: Rect,
    no_color: bool,
    frame: &mut Frame,
) {
    let visible_h = rect.height.saturating_sub(2) as usize;
    let chunks =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rect);
    let left_rect = chunks[0];
    let right_rect = chunks[1];

    // Build parallel left/right line lists from the unified diff.
    let lang = detect_diff_lang(lines).unwrap_or("");
    let mut left: Vec<Line<'static>> = Vec::new();
    let mut right: Vec<Line<'static>> = Vec::new();
    for l in lines.iter().skip(scroll).take(visible_h * 2) {
        if !l.starts_with("---") && l.starts_with('-') {
            left.push(highlighted_diff_line(l, false, lang, no_color));
            right.push(Line::raw(String::new()));
        } else if !l.starts_with("+++") && l.starts_with('+') {
            left.push(Line::raw(String::new()));
            right.push(highlighted_diff_line(l, true, lang, no_color));
        } else {
            // Context or hunk header — appears on both sides.
            let styled = Line::from(vec![Span::raw(l.clone())]);
            left.push(styled.clone());
            right.push(styled);
        }
    }

    let left_widget = Paragraph::new(left.into_iter().take(visible_h).collect::<Vec<_>>()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" old  [S] unified "),
    );
    let right_widget = Paragraph::new(right.into_iter().take(visible_h).collect::<Vec<_>>())
        .block(Block::default().borders(Borders::ALL).title(" new "));

    frame.render_widget(left_widget, left_rect);
    frame.render_widget(right_widget, right_rect);
}

/// Extracts a file extension hint from unified diff header lines (`--- a/...`
/// / `+++ b/...`).  Returns `None` when no recognisable header is found.
pub fn detect_diff_lang(lines: &[String]) -> Option<&'static str> {
    for l in lines {
        let path = if let Some(rest) = l.strip_prefix("--- a/").or_else(|| l.strip_prefix("--- ")) {
            rest
        } else if let Some(rest) = l.strip_prefix("+++ b/").or_else(|| l.strip_prefix("+++ ")) {
            rest
        } else {
            continue;
        };
        if let Some(ext) = path.rsplit('.').next() {
            // Map common extensions to syntect-recognised tokens.
            let token: &'static str = match ext {
                "rs" => "rs",
                "py" => "py",
                "js" | "mjs" | "cjs" => "js",
                "ts" | "tsx" => "ts",
                "json" => "json",
                "toml" => "toml",
                "yaml" | "yml" => "yaml",
                "sh" | "bash" => "sh",
                "go" => "go",
                "c" | "h" => "c",
                "cpp" | "cc" | "cxx" | "hpp" => "cpp",
                "md" => "md",
                _ => continue,
            };
            return Some(token);
        }
    }
    None
}

/// Returns a styled [`Line`] for a single `+` or `-` diff line.
///
/// When `no_color` is false and `lang` is non-empty, the content portion is
/// syntax-highlighted via syntect.  The leading sigil keeps the standard
/// add/remove colour so the diff structure remains visually clear.
pub fn highlighted_diff_line(
    raw: &str,
    is_added: bool,
    lang: &str,
    no_color: bool,
) -> Line<'static> {
    let p = palette();
    let (sigil, sigil_style) = if is_added {
        ("+", Style::default().fg(p.code_added))
    } else {
        ("-", Style::default().fg(p.code_removed))
    };

    // Strip the leading sigil character.
    let content = raw.get(1..).unwrap_or("");

    if no_color || lang.is_empty() {
        let text = format!("{sigil}{content}");
        return Line::from(vec![Span::styled(text, sigil_style)]);
    }

    // Highlight via the tree-sitter/syntect dispatch — first (and only) line.
    let highlighted = highlight_code(lang, content);
    let mut spans: Vec<Span<'static>> = vec![Span::styled(sigil, sigil_style)];
    if let Some(styled_line) = highlighted.into_iter().next() {
        if let Some(line) = styled_line.spans {
            spans.extend(line.spans);
        } else {
            spans.push(Span::raw(styled_line.text));
        }
    } else {
        spans.push(Span::raw(content.to_owned()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn make_term(w: u16, h: u16) -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(w, h)).unwrap()
    }

    fn sample_diff() -> Vec<String> {
        vec![
            "--- a/foo.rs".into(),
            "+++ b/foo.rs".into(),
            "@@ -1,3 +1,3 @@".into(),
            " context line".into(),
            "-old line".into(),
            "+new line".into(),
        ]
    }

    #[test]
    fn is_diff_content_detects_unified_diff() {
        assert!(is_diff_content(&sample_diff()));
    }

    #[test]
    fn is_diff_content_rejects_plain_text() {
        let plain: Vec<String> = vec!["hello".into(), "world".into()];
        assert!(!is_diff_content(&plain));
    }

    #[test]
    fn render_unified_no_panic() {
        let mut t = make_term(80, 20);
        let area = Rect::new(0, 0, 80, 20);
        t.draw(|f| render_unified(&sample_diff(), 0, area, true, f))
            .unwrap();
    }

    #[test]
    fn render_split_no_panic() {
        let mut t = make_term(80, 20);
        let area = Rect::new(0, 0, 80, 20);
        t.draw(|f| render_split(&sample_diff(), 0, area, true, f))
            .unwrap();
    }

    #[test]
    fn render_unified_with_scroll_no_panic() {
        let mut t = make_term(80, 20);
        let area = Rect::new(0, 0, 80, 20);
        t.draw(|f| render_unified(&sample_diff(), 3, area, true, f))
            .unwrap();
    }

    #[test]
    fn detect_diff_lang_from_rust_header() {
        let lines: Vec<String> = vec!["--- a/src/main.rs".into(), "+++ b/src/main.rs".into()];
        assert_eq!(detect_diff_lang(&lines), Some("rs"));
    }

    #[test]
    fn detect_diff_lang_returns_none_for_unknown() {
        let lines: Vec<String> = vec!["@@ -1 +1 @@".into()];
        assert_eq!(detect_diff_lang(&lines), None);
    }

    #[test]
    fn highlighted_diff_line_produces_multi_span_for_added() {
        let line = "+let x = 1;";
        let result = highlighted_diff_line(line, true, "rs", false);
        // Must not panic and must produce spans (content highlighted)
        assert!(!result.spans.is_empty());
    }

    #[test]
    fn highlighted_diff_line_falls_back_to_mono_when_no_color() {
        let line = "+let x = 1;";
        let result = highlighted_diff_line(line, true, "rs", true);
        assert!(!result.spans.is_empty());
    }
}
