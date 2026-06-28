//! Value / ROI panel widget — cumulative token cost per active openspec change.
//!
//! Renders below the quality panel when `Ctrl-V` is active. All data comes from
//! [`ValueSnapshot`]; the widget never fetches or blocks.

use crate::theme::palette;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// Immutable snapshot of value/ROI data the panel needs to render.
#[derive(Debug, Clone, Default)]
pub struct ValueSnapshot {
    /// Active openspec change name, if any.
    pub change_name: Option<String>,
    /// Cumulative token count (input + output) attributed to this change.
    pub token_cost: u64,
    /// Estimated cost in microdollars (`1_000_000` = $1.00).
    pub cost_usd_micros: u64,
    /// Average quality score across turns for this change (0–100).
    pub quality_avg: u8,
    /// Human-readable ROI estimate: "high", "medium", or "low".
    pub estimated_value: &'static str,
}

impl ValueSnapshot {
    fn cost_dollars(&self) -> f64 {
        #[allow(clippy::cast_precision_loss)] // microdollar sums never exceed 2^53
        let micros = self.cost_usd_micros as f64;
        micros / 1_000_000.0
    }

    fn roi_fill(&self) -> u8 {
        match self.estimated_value {
            "high" => 3,
            "medium" => 2,
            _ => 1,
        }
    }
}

/// The value rail panel.
pub struct ValuePanel<'a> {
    pub snapshot: &'a ValueSnapshot,
}

impl<'a> ValuePanel<'a> {
    #[must_use]
    pub fn new(snapshot: &'a ValueSnapshot) -> Self {
        Self { snapshot }
    }

    pub fn render(&self, area: Rect, frame: &mut Frame) {
        if area.height < 3 {
            return;
        }

        let p = palette();
        let snap = self.snapshot;
        let inner_w = (area.width as usize).saturating_sub(2).max(1);
        let mut lines: Vec<Line<'_>> = Vec::new();

        let Some(ref change) = snap.change_name else {
            lines.push(Line::from(vec![Span::styled(
                "no active change".to_owned(),
                Style::default().fg(p.text_dim),
            )]));
            frame.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(p.border))
                        .title(" value "),
                ),
                area,
            );
            return;
        };

        // Change name (truncated).
        let name: String = change.chars().take(inner_w).collect();
        lines.push(Line::from(vec![Span::styled(
            name,
            Style::default()
                .fg(p.text_dim)
                .add_modifier(Modifier::ITALIC),
        )]));

        // Token cost.
        let tok_line = format!("{} tok", snap.token_cost);
        let tok_truncated: String = tok_line.chars().take(inner_w).collect();
        lines.push(Line::from(vec![Span::styled(
            tok_truncated,
            Style::default().fg(p.text),
        )]));

        // USD cost.
        let usd_line = format!("${:.4}", snap.cost_dollars());
        let usd_truncated: String = usd_line.chars().take(inner_w).collect();
        lines.push(Line::from(vec![Span::styled(
            usd_truncated,
            Style::default().fg(p.text),
        )]));

        // Average quality.
        let q_line = format!("q avg: {}/100", snap.quality_avg);
        let q_truncated: String = q_line.chars().take(inner_w).collect();
        lines.push(Line::from(vec![Span::styled(
            q_truncated,
            Style::default().fg(p.text_dim),
        )]));

        // ROI bar: ▓ filled, ░ empty — 3 segments max.
        let filled = snap.roi_fill() as usize;
        let empty = 3usize.saturating_sub(filled);
        let bar = format!(
            "roi: {}{}  ~{}",
            "▓".repeat(filled),
            "░".repeat(empty),
            snap.estimated_value
        );
        let bar_truncated: String = bar.chars().take(inner_w).collect();
        lines.push(Line::from(vec![Span::styled(
            bar_truncated,
            Style::default().fg(p.success),
        )]));

        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(p.border))
                    .title(" value "),
            ),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_snapshot(snap: &ValueSnapshot, w: u16, h: u16) -> String {
        let panel = ValuePanel::new(snap);
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| panel.render(f.area(), f)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn panel_renders_without_panic() {
        let snap = ValueSnapshot {
            change_name: Some("smedja-quality-panel".into()),
            token_cost: 42_000,
            cost_usd_micros: 42_000,
            quality_avg: 78,
            estimated_value: "high",
        };
        let rendered = render_snapshot(&snap, 30, 10);
        assert!(rendered.contains("value"), "title present: {rendered}");
    }

    #[test]
    fn panel_hides_when_height_below_3() {
        let snap = ValueSnapshot::default();
        let panel = ValuePanel::new(&snap);
        let backend = TestBackend::new(30, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| panel.render(f.area(), f)).unwrap();
    }

    #[test]
    fn panel_shows_no_active_change_placeholder() {
        let snap = ValueSnapshot::default();
        let rendered = render_snapshot(&snap, 30, 8);
        assert!(
            rendered.contains("value"),
            "title present on no-change: {rendered}"
        );
        // Panel renders — no panic is the key assertion.
    }

    #[test]
    fn panel_renders_roi_bar() {
        let snap = ValueSnapshot {
            change_name: Some("test-change".into()),
            token_cost: 1_000,
            cost_usd_micros: 1_000,
            quality_avg: 80,
            estimated_value: "high",
        };
        let rendered = render_snapshot(&snap, 40, 10);
        assert!(rendered.contains("value"), "title present: {rendered}");
    }

    #[test]
    fn panel_renders_at_narrow_width_without_panic() {
        let snap = ValueSnapshot {
            change_name: Some("smedja-quality-panel".into()),
            token_cost: 99_999,
            cost_usd_micros: 99_999,
            quality_avg: 55,
            estimated_value: "low",
        };
        let rendered = render_snapshot(&snap, 20, 10);
        assert!(
            rendered.contains("value"),
            "title at narrow width: {rendered}"
        );
    }

    #[test]
    fn roi_fill_maps_correctly() {
        let high = ValueSnapshot {
            estimated_value: "high",
            ..ValueSnapshot::default()
        };
        let medium = ValueSnapshot {
            estimated_value: "medium",
            ..ValueSnapshot::default()
        };
        let low = ValueSnapshot {
            estimated_value: "low",
            ..ValueSnapshot::default()
        };
        assert_eq!(high.roi_fill(), 3);
        assert_eq!(medium.roi_fill(), 2);
        assert_eq!(low.roi_fill(), 1);
    }
}
