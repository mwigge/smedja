//! Diff viewer — unified and side-by-side rendering for diff overlays.

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
    let rendered: Vec<Line<'_>> = lines
        .iter()
        .skip(scroll)
        .take(visible_h)
        .map(|l| line_to_styled(l, no_color))
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
    let mut left: Vec<Line<'_>> = Vec::new();
    let mut right: Vec<Line<'_>> = Vec::new();
    for l in lines.iter().skip(scroll).take(visible_h * 2) {
        if l.starts_with('-') {
            left.push(line_to_styled(l, no_color));
            right.push(Line::raw(String::new()));
        } else if l.starts_with('+') {
            left.push(Line::raw(String::new()));
            right.push(line_to_styled(l, no_color));
        } else {
            // Context or hunk header — appears on both sides.
            left.push(line_to_styled(l, no_color));
            right.push(line_to_styled(l, no_color));
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

/// Colours a single diff line by its sigil (`+`, `-`, `@@`, context).
fn line_to_styled(l: &str, no_color: bool) -> Line<'_> {
    if no_color {
        return Line::raw(l);
    }
    let p = palette();
    let style = if l.starts_with('+') && !l.starts_with("+++") {
        Style::default().fg(p.code_added)
    } else if l.starts_with('-') && !l.starts_with("---") {
        Style::default().fg(p.code_removed)
    } else if l.starts_with("@@") {
        Style::default().fg(p.accent)
    } else {
        Style::default()
    };
    Line::from(Span::styled(l.to_owned(), style))
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
}
