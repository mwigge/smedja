//! Plan step tracker panel.
//!
//! Parses numbered list items (`1. step text`) from the assistant's streaming
//! response and renders them as a live plan tracker in the right rail.
//! Requires at least 2 consecutive numbered items to display, so single
//! numbered sentences in prose are ignored.

use crate::theme::palette;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// Parses numbered list items from `text`, starting at item number
/// `offset + 1` (so repeated calls accumulate without re-scanning the full
/// text each time).
///
/// Returns the new steps found beyond `offset` existing ones.  Requires the
/// list to be contiguous (1, 2, 3…) — a gap resets detection.
#[must_use]
pub fn extract_new_steps(text: &str, offset: usize) -> Vec<String> {
    let all = extract_all_steps(text);
    if all.len() > offset {
        all.into_iter().skip(offset).collect()
    } else {
        vec![]
    }
}

/// Extracts all numbered list items from `text`.
///
/// Returns an empty `Vec` when fewer than 2 items are found, so a single
/// incidental numbered sentence is not treated as a plan.
#[must_use]
pub fn extract_all_steps(text: &str) -> Vec<String> {
    let mut steps: Vec<String> = Vec::new();
    let mut expected = 1u32;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some((n, rest)) = parse_numbered_item(trimmed) {
            if n == expected {
                steps.push(rest.to_owned());
                expected += 1;
            } else if n == 1 {
                // New list started — discard any partial prior list.
                steps.clear();
                steps.push(rest.to_owned());
                expected = 2;
            } else {
                // Gap in the sequence — discard and keep looking.
                steps.clear();
                expected = 1;
            }
        }
    }

    if steps.len() >= 2 {
        steps
    } else {
        vec![]
    }
}

/// Parses `"N. text"` from the start of `s`.
///
/// Returns `(N, text)` on success; `None` if `s` does not start with a
/// digit followed by `. `.
fn parse_numbered_item(s: &str) -> Option<(u32, &str)> {
    let digit_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digit_end == 0 {
        return None;
    }
    let n: u32 = s[..digit_end].parse().ok()?;
    let rest = s[digit_end..].strip_prefix(". ")?.trim_start();
    if rest.is_empty() {
        return None;
    }
    Some((n, rest))
}

/// Height (rows) needed to render `step_count` plan steps including borders.
#[must_use]
pub fn panel_height(step_count: usize) -> u16 {
    // Header + border top/bottom + one row per step, capped at 14.
    u16::try_from(step_count + 2).unwrap_or(14).min(14)
}

/// Plan tracker panel widget.
pub struct PlanPanel<'a> {
    pub steps: &'a [String],
}

impl<'a> PlanPanel<'a> {
    #[must_use]
    pub fn new(steps: &'a [String]) -> Self {
        Self { steps }
    }

    pub fn render(&self, area: Rect, frame: &mut Frame) {
        if area.height < 3 || self.steps.is_empty() {
            return;
        }
        let p = palette();
        let inner_w = (area.width as usize).saturating_sub(2).max(1);
        let mut lines: Vec<Line<'_>> = Vec::new();

        for (i, step) in self.steps.iter().enumerate() {
            let n = i + 1;
            let label = format!("{n}. ");
            let text_budget = inner_w.saturating_sub(label.len());
            let display = if step.len() > text_budget {
                let end = crate::floor_char_boundary(step, text_budget.saturating_sub(1));
                format!("{}…", &step[..end])
            } else {
                step.clone()
            };
            lines.push(Line::from(vec![
                Span::styled(
                    label,
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
                Span::raw(display),
            ]));
        }

        let block = Block::default()
            .title(" plan ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.border));

        frame.render_widget(Paragraph::new(lines).block(block), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_single_item_returns_empty() {
        let text = "1. do the thing\nsome other text";
        assert!(extract_all_steps(text).is_empty());
    }

    #[test]
    fn extract_two_items_returns_both() {
        let text = "1. first step\n2. second step";
        assert_eq!(extract_all_steps(text), vec!["first step", "second step"]);
    }

    #[test]
    fn extract_ignores_gap_in_sequence() {
        // 1, 2, 4 — gap means incomplete list
        let text = "1. one\n2. two\n4. four";
        assert!(extract_all_steps(text).is_empty());
    }

    #[test]
    fn extract_resets_on_new_list_start() {
        // First 1. then 1. 2. — second list wins
        let text = "1. old start\nsome prose\n1. new start\n2. new second";
        let steps = extract_all_steps(text);
        assert_eq!(steps, vec!["new start", "new second"]);
    }

    #[test]
    fn extract_strips_leading_whitespace_from_text() {
        let text = "  1. indented step\n  2. also indented";
        let steps = extract_all_steps(text);
        assert_eq!(steps, vec!["indented step", "also indented"]);
    }

    #[test]
    fn extract_new_steps_returns_only_new_items() {
        let text = "1. one\n2. two\n3. three";
        let new = extract_new_steps(text, 2);
        assert_eq!(new, vec!["three"]);
    }

    #[test]
    fn extract_new_steps_with_zero_offset_returns_all() {
        let text = "1. one\n2. two";
        let new = extract_new_steps(text, 0);
        assert_eq!(new, vec!["one", "two"]);
    }

    #[test]
    fn parse_numbered_item_parses_simple_item() {
        assert_eq!(parse_numbered_item("3. do thing"), Some((3, "do thing")));
    }

    #[test]
    fn parse_numbered_item_rejects_non_numbered() {
        assert!(parse_numbered_item("- bullet item").is_none());
        assert!(parse_numbered_item("prose text").is_none());
        assert!(parse_numbered_item("").is_none());
    }

    #[test]
    fn panel_height_caps_at_fourteen() {
        assert_eq!(panel_height(20), 14);
        assert_eq!(panel_height(3), 5); // 3 steps + 2 borders
    }

    // Regression: a long multibyte step once panicked in `&step[..budget]` when
    // the truncation index fell mid-codepoint. It must now floor to a boundary.
    #[test]
    fn multibyte_step_truncation_renders_without_panic() {
        use ratatui::{backend::TestBackend, Terminal};
        let steps = vec![
            "café_αβγ_日本語_ελληνικά_привет_мир_こんにちは_世界".to_owned(),
            "second_αβγ_日本語_step".to_owned(),
        ];
        let panel = PlanPanel::new(&steps);
        let backend = TestBackend::new(20, 8);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| panel.render(f.area(), f)).unwrap();
    }
}
