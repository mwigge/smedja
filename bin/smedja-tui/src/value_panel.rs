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
    /// Rolling history of `token_cost` samples (most recent last, capped at 64)
    /// feeding the spend-velocity sparkline.
    pub tok_trend: Vec<u64>,
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

        // Spend velocity: deltas of the rolling token-cost history. Needs at
        // least two samples to form a delta; the cumulative counter resets when
        // a new change starts, so saturate rather than underflow.
        if snap.tok_trend.len() >= 2 {
            let deltas: Vec<u64> = snap
                .tok_trend
                .windows(2)
                .map(|w| w[1].saturating_sub(w[0]))
                .collect();
            let max = deltas.iter().copied().max().unwrap_or(0).max(1);
            let pct: Vec<u8> = deltas
                .iter()
                .map(|&d| u8::try_from(d.saturating_mul(100) / max).unwrap_or(100))
                .collect();
            let spd = format!("spd {}", crate::viz::sparkline(&pct, 100));
            let spd_truncated: String = spd.chars().take(inner_w).collect();
            lines.push(Line::from(vec![
                Span::styled("spd ", Style::default().fg(p.text_dim)),
                Span::styled(
                    spd_truncated.chars().skip(4).collect::<String>(),
                    Style::default().fg(p.accent),
                ),
            ]));
        }

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
            tok_trend: Vec::new(),
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
            tok_trend: Vec::new(),
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
            tok_trend: Vec::new(),
        };
        let rendered = render_snapshot(&snap, 20, 10);
        assert!(
            rendered.contains("value"),
            "title at narrow width: {rendered}"
        );
    }

    #[test]
    fn spend_velocity_shown_with_two_or_more_samples() {
        let snap = ValueSnapshot {
            change_name: Some("test-change".into()),
            token_cost: 300,
            cost_usd_micros: 300,
            quality_avg: 80,
            estimated_value: "high",
            tok_trend: vec![100, 300],
        };
        let rendered = render_snapshot(&snap, 30, 12);
        assert!(rendered.contains("spd"), "spd line present: {rendered}");
    }

    #[test]
    fn spend_velocity_hidden_below_two_samples() {
        let snap = ValueSnapshot {
            change_name: Some("test-change".into()),
            token_cost: 100,
            cost_usd_micros: 100,
            quality_avg: 80,
            estimated_value: "high",
            tok_trend: vec![100],
        };
        let rendered = render_snapshot(&snap, 30, 12);
        assert!(!rendered.contains("spd"), "no spd line: {rendered}");
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
