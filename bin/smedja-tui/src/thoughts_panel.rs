//! Thinking-token display — spinner indicator, step timeline overlay.

use crate::theme::palette;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// A single timestamped step in the thinking timeline for a turn.
///
/// Steps are accumulated during a turn and displayed in the expanded thinking
/// overlay as a linear trace from the model's first reasoning token to the
/// final answer delivery.
#[derive(Debug, Clone)]
pub enum ThinkingStep {
    /// Extended thinking text from the model (Claude extended thinking).
    Reasoning { text: String, elapsed_s: f32 },
    /// A tool was invoked by the model.
    Tool {
        name: String,
        preview: String,
        elapsed_s: f32,
    },
    /// The answer was delivered (turn done).
    Answer { elapsed_s: f32 },
}

impl ThinkingStep {
    /// Elapsed seconds since turn start when this step occurred.
    #[must_use]
    pub fn elapsed_s(&self) -> f32 {
        match self {
            Self::Reasoning { elapsed_s, .. }
            | Self::Tool { elapsed_s, .. }
            | Self::Answer { elapsed_s } => *elapsed_s,
        }
    }
}

/// Renders a floating overlay showing the turn's thinking step timeline when
/// `thinking_expanded` is true.  Each step is shown as a timestamped, color-
/// coded line.  No-op when `steps` is empty or `area` is too small.
pub fn render_step_overlay(
    area: Rect,
    thinking_expanded: bool,
    steps: &[ThinkingStep],
    no_color: bool,
    frame: &mut Frame,
) {
    if !thinking_expanded || steps.is_empty() || area.height < 4 {
        return;
    }
    let visible = area.height.min(16);
    let overlay_rect = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(visible + 1),
        area.width,
        visible,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" trace (T to collapse) ");
    let inner = block.inner(overlay_rect);

    let p = palette();
    let (dim, accent, bright) = if no_color {
        (Style::default(), Style::default(), Style::default())
    } else {
        (
            Style::default()
                .fg(p.text_dim)
                .add_modifier(Modifier::ITALIC),
            Style::default().fg(p.accent),
            Style::default().fg(p.text),
        )
    };

    let lines: Vec<Line<'_>> =
        steps
            .iter()
            .map(|step| {
                let ts = format!("[+{:.1}s] ", step.elapsed_s());
                match step {
                    ThinkingStep::Reasoning { text, .. } => {
                        let preview: String = text
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(inner.width.saturating_sub(12) as usize)
                            .collect();
                        Line::from(vec![
                            Span::styled(ts, dim),
                            Span::styled("\u{1f4ad} ", dim),
                            Span::styled(preview, dim),
                        ])
                    }
                    ThinkingStep::Tool { name, preview, .. } => {
                        let arg: String =
                            preview
                                .chars()
                                .take(inner.width.saturating_sub(
                                    u16::try_from(14 + name.len()).unwrap_or(u16::MAX),
                                ) as usize)
                                .collect();
                        Line::from(vec![
                            Span::styled(ts, accent),
                            Span::styled("\u{2318} ", accent),
                            Span::styled(format!("{name}: {arg}"), accent),
                        ])
                    }
                    ThinkingStep::Answer { .. } => Line::from(vec![
                        Span::styled(ts, bright),
                        Span::styled("\u{25ce} answer", bright),
                    ]),
                }
            })
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

    #[test]
    fn thinking_step_renders_timeline_without_panic() {
        let steps = vec![
            ThinkingStep::Reasoning {
                text: "why do this?".into(),
                elapsed_s: 0.1,
            },
            ThinkingStep::Tool {
                name: "bash".into(),
                preview: "ls /src".into(),
                elapsed_s: 0.5,
            },
            ThinkingStep::Answer { elapsed_s: 1.2 },
        ];
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_step_overlay(area, true, &steps, true, f);
            })
            .unwrap();
    }

    #[test]
    fn render_step_overlay_no_panic_when_empty_steps() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_step_overlay(area, true, &[], true, f);
            })
            .unwrap();
    }

    #[test]
    fn render_step_overlay_skips_when_not_expanded() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                let steps = vec![ThinkingStep::Answer { elapsed_s: 0.5 }];
                render_step_overlay(area, false, &steps, true, f);
            })
            .unwrap();
    }
}
