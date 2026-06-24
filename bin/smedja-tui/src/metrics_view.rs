//! Metrics view widget — a read-only panel showing per-runner tokens, cost, and
//! error counts for the latest rollup window.
//!
//! Mirrors the [`ContextRail`](crate::context_rail) precedent: a small,
//! toggleable, read-only widget that renders from a cached snapshot. The
//! snapshot is the per-runner aggregate of the most recent `metrics.summary`
//! window; this widget never fetches or mutates — it only renders what the app
//! already holds, keeping it off the render hot path.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget};

/// One per-runner aggregate row for the latest rollup window.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricsRow {
    /// Runner name (e.g. `"claude"`, `"local"`).
    pub runner: String,
    /// Total input + output tokens for this runner in the window.
    pub tokens: i64,
    /// Total cost in USD for this runner in the window.
    pub cost_usd: f64,
    /// Total error count for this runner in the window.
    pub errors: i64,
}

/// The metrics view widget — renders a cached per-runner snapshot.
pub struct MetricsView {
    /// Per-runner rows for the latest window, in display order.
    pub rows: Vec<MetricsRow>,
}

impl MetricsView {
    /// Width of the panel when shown as a right-hand sidebar.
    pub const WIDTH: u16 = 38;

    /// Wraps `rows` for rendering.
    #[must_use]
    pub fn new(rows: Vec<MetricsRow>) -> Self {
        Self { rows }
    }

    /// Renders the panel content as text lines (header + one row per runner, or
    /// a placeholder when there is no data). Exposed for unit testing the layout
    /// without a `Buffer`.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!(
            "{:<10} {:>8} {:>9} {:>5}",
            "RUNNER", "TOKENS", "COST", "ERR"
        )];
        if self.rows.is_empty() {
            out.push("(no metrics)".to_owned());
            return out;
        }
        for row in &self.rows {
            let runner = &row.runner[..row.runner.len().min(10)];
            out.push(format!(
                "{:<10} {:>8} {:>9} {:>5}",
                runner,
                row.tokens,
                format!("${:.4}", row.cost_usd),
                row.errors,
            ));
        }
        out
    }
}

impl Widget for MetricsView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let lines: Vec<Line<'_>> = self
            .lines()
            .into_iter()
            .enumerate()
            .map(|(idx, text)| {
                if idx == 0 {
                    Line::from(Span::styled(
                        text,
                        Style::default().add_modifier(Modifier::BOLD),
                    ))
                } else {
                    // Highlight any runner with errors in red.
                    let has_error = self.rows.get(idx - 1).is_some_and(|row| row.errors > 0);
                    let style = if has_error {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default()
                    };
                    Line::from(Span::styled(text, style))
                }
            })
            .collect();
        Paragraph::new(Text::from(lines)).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_show_placeholder_when_empty() {
        let view = MetricsView::new(vec![]);
        let lines = view.lines();
        assert_eq!(lines.len(), 2, "header + placeholder");
        assert!(lines[0].contains("RUNNER") && lines[0].contains("ERR"));
        assert!(lines[1].contains("(no metrics)"));
    }

    #[test]
    fn lines_render_per_runner_cost_tokens_and_errors() {
        let view = MetricsView::new(vec![
            MetricsRow {
                runner: "claude".into(),
                tokens: 780,
                cost_usd: 0.06,
                errors: 1,
            },
            MetricsRow {
                runner: "local".into(),
                tokens: 480,
                cost_usd: 0.0,
                errors: 0,
            },
        ]);
        let lines = view.lines();
        assert_eq!(lines.len(), 3, "header + two runners");
        assert!(lines[1].contains("claude"));
        assert!(lines[1].contains("780"));
        assert!(lines[1].contains("$0.0600"));
        assert!(lines[1].contains('1'), "claude error count shown");
        assert!(lines[2].contains("local"));
        assert!(lines[2].contains("$0.0000"));
    }

    #[test]
    fn renders_into_buffer_without_panicking() {
        let view = MetricsView::new(vec![MetricsRow {
            runner: "claude".into(),
            tokens: 100,
            cost_usd: 0.01,
            errors: 0,
        }]);
        let area = Rect::new(0, 0, MetricsView::WIDTH, 5);
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf);
        // The header label must appear somewhere in the rendered buffer.
        let rendered: String = buf
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(rendered.contains("RUNNER"));
    }
}
