//! Thinking-token display — spinner indicator and expanded overlay.

use crate::theme::palette;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Renders a one-line spinner at the bottom of `area` while a turn is in
/// flight.  Advances `spinner_tick` each call.  When `current_thinking` is
/// non-empty it shows a trailing preview of the last ~50 chars.  When
/// `pending_tool` is set it shows the tool name and truncated input instead.
pub fn render_indicator(
    area: Rect,
    turn_in_flight: bool,
    spinner_tick: &mut u8,
    current_thinking: &str,
    pending_tool: Option<&(usize, String, String)>,
    no_color: bool,
    frame: &mut Frame,
) {
    if !turn_in_flight || area.height < 1 {
        return;
    }
    let frame_char = SPINNER[*spinner_tick as usize % SPINNER.len()];
    *spinner_tick = spinner_tick.wrapping_add(1);

    let indicator_area = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(1),
        area.width,
        1,
    );

    let p = palette();
    let spinner_style = if no_color {
        Style::default()
    } else {
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
    };
    let dim_style = if no_color {
        Style::default()
    } else {
        Style::default()
            .fg(p.text_dim)
            .add_modifier(Modifier::ITALIC)
    };

    let (label, show_preview) = if let Some((_, name, inp)) = pending_tool {
        let inp_short: String = inp.chars().take(40).collect();
        let ellipsis = if inp.chars().count() > 40 {
            "\u{2026}"
        } else {
            ""
        };
        (format!("{frame_char} {name}: {inp_short}{ellipsis}"), false)
    } else if !current_thinking.is_empty() {
        (format!("{frame_char} thinking\u{2026}"), true)
    } else {
        (format!("{frame_char} working\u{2026}"), false)
    };

    let mut spans = vec![Span::styled(label, spinner_style)];
    if show_preview {
        let preview: String = current_thinking
            .chars()
            .rev()
            .take(50)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let preview = preview.replace('\n', " ");
        spans.push(Span::raw("  "));
        spans.push(Span::styled(preview, dim_style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), indicator_area);
}

/// Renders a floating overlay showing the full `current_thinking` text when
/// `thinking_expanded` is true.  No-op if `current_thinking` is empty or
/// `area` is too small.
pub fn render_overlay(
    area: Rect,
    thinking_expanded: bool,
    current_thinking: &str,
    no_color: bool,
    frame: &mut Frame,
) {
    if !thinking_expanded || current_thinking.is_empty() || area.height < 4 {
        return;
    }
    let h = area.height.min(10);
    let overlay_rect = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(h + 1),
        area.width,
        h,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" thinking (T to collapse) ");
    let inner = block.inner(overlay_rect);

    let p = palette();
    let thinking_style = if no_color {
        Style::default()
    } else {
        Style::default().fg(p.text_dim)
    };
    let lines: Vec<Line<'_>> = current_thinking
        .lines()
        .map(|l| Line::from(Span::styled(l.to_owned(), thinking_style)))
        .collect();
    frame.render_widget(block, overlay_rect);
    frame.render_widget(
        Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn make_frame_area(w: u16, h: u16) -> (Terminal<TestBackend>, Rect) {
        let backend = TestBackend::new(w, h);
        let terminal = Terminal::new(backend).unwrap();
        let area = Rect::new(0, 0, w, h);
        (terminal, area)
    }

    #[test]
    fn render_indicator_no_panic_when_turn_in_flight() {
        let (mut terminal, area) = make_frame_area(80, 10);
        terminal
            .draw(|f| {
                let mut tick: u8 = 0;
                render_indicator(area, true, &mut tick, "", None, true, f);
                assert_eq!(tick, 1);
            })
            .unwrap();
    }

    #[test]
    fn render_indicator_skips_when_not_in_flight() {
        let (mut terminal, area) = make_frame_area(80, 10);
        terminal
            .draw(|f| {
                let mut tick: u8 = 5;
                render_indicator(area, false, &mut tick, "some thoughts", None, true, f);
                // tick must not advance when not in flight
                assert_eq!(tick, 5);
            })
            .unwrap();
    }

    #[test]
    fn render_overlay_no_panic_when_expanded() {
        let (mut terminal, area) = make_frame_area(80, 20);
        terminal
            .draw(|f| {
                render_overlay(area, true, "some thinking text", true, f);
            })
            .unwrap();
    }

    #[test]
    fn render_overlay_skips_when_not_expanded() {
        let (mut terminal, area) = make_frame_area(80, 20);
        // should not panic; nothing is rendered
        terminal
            .draw(|f| {
                render_overlay(area, false, "thoughts", true, f);
            })
            .unwrap();
    }

    #[test]
    fn render_overlay_skips_when_area_too_small() {
        let (mut terminal, area) = make_frame_area(80, 3);
        terminal
            .draw(|f| {
                render_overlay(area, true, "thoughts", true, f);
            })
            .unwrap();
    }
}
