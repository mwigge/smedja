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

/// Computes the change's USD cost in microdollars using the session's blended
/// rate ($/token) applied to the change's own cumulative token count.
///
/// The session rate is `session_cost_usd / session_tokens_total`; multiplying by
/// the change's `change_token_cost` attributes a real slice of session spend to
/// the change. Returns 0 when there is not enough session data to derive a rate.
#[must_use]
pub fn blended_cost_micros(
    session_cost_usd: f64,
    session_tokens_total: u64,
    change_token_cost: u64,
) -> u64 {
    if session_tokens_total == 0 || session_cost_usd <= 0.0 {
        return 0;
    }
    #[allow(clippy::cast_precision_loss)] // token counts stay well under 2^53
    let rate_num = change_token_cost as f64;
    #[allow(clippy::cast_precision_loss)]
    let rate_den = session_tokens_total as f64;
    let usd = session_cost_usd * (rate_num / rate_den);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let micros = (usd * 1_000_000.0) as u64;
    micros
}

/// Maps a running-average quality score (0–100) to a coarse ROI estimate:
/// `>=80` → `"high"`, `>=60` → `"medium"`, otherwise `"low"`.
#[must_use]
pub fn estimate_value(quality_avg: u8) -> &'static str {
    if quality_avg >= 80 {
        "high"
    } else if quality_avg >= 60 {
        "medium"
    } else {
        "low"
    }
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

    /// Colour for the ROI / value figure: green "high", amber "medium", red
    /// "low" — the same green→amber→red reading used across the rail.
    fn value_color(&self) -> ratatui::style::Color {
        let p = palette();
        match self.estimated_value {
            "high" => p.success,
            "medium" => p.warn,
            _ => p.error,
        }
    }
}

/// Compact token count: `1.2k`, `3.4M`, or the raw number below 1000.
#[allow(clippy::cast_precision_loss)]
fn fmt_tok(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
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
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.border))
            .title(" value ");

        // ── Empty state ──────────────────────────────────────────────────────
        // ROI is scoped to an openspec change; without one there is nothing to
        // attribute spend to. Say so plainly rather than showing an empty meter.
        let Some(ref change) = snap.change_name else {
            lines.push(Line::from(Span::styled(
                "no active change",
                Style::default().fg(p.text_dim),
            )));
            lines.push(Line::from(Span::styled(
                "ROI tracks openspec/changes",
                Style::default().fg(p.text_dim),
            )));
            frame.render_widget(Paragraph::new(lines).block(block), area);
            return;
        };

        // Change name (dim italic, truncated).
        let name: String = change.chars().take(inner_w).collect();
        lines.push(Line::from(Span::styled(
            name,
            Style::default()
                .fg(p.text_dim)
                .add_modifier(Modifier::ITALIC),
        )));

        // ── ROI figure + gauge (molten-accent meter, 3 segments) ─────────────
        let roi_color = snap.value_color();
        let roi_w = inner_w.saturating_sub(10).clamp(3, 10);
        let roi_gauge = crate::viz::microbar(f64::from(snap.roi_fill()), 3.0, roi_w);
        lines.push(Line::from(vec![
            Span::styled("ROI ", Style::default().fg(p.text_dim)),
            Span::styled(roi_gauge, Style::default().fg(p.molten)),
            Span::styled(
                format!(" {}", snap.estimated_value),
                Style::default().fg(roi_color).add_modifier(Modifier::BOLD),
            ),
        ]));

        // ── Cost-vs-value micro-bar: quality-avg as the value delivered ──────
        let val_w = inner_w.saturating_sub(8).clamp(3, 10);
        let val_gauge = crate::viz::microbar(f64::from(snap.quality_avg), 100.0, val_w);
        lines.push(Line::from(vec![
            Span::styled("val ", Style::default().fg(p.text_dim)),
            Span::styled(val_gauge, Style::default().fg(roi_color)),
            Span::styled(
                format!(" q{}", snap.quality_avg),
                Style::default().fg(p.text_dim),
            ),
        ]));

        // ── Cost + tokens ────────────────────────────────────────────────────
        lines.push(Line::from(vec![
            Span::styled(
                format!("${:.4}", snap.cost_dollars()),
                Style::default().fg(p.text),
            ),
            Span::styled(
                format!("  {} tok", fmt_tok(snap.token_cost)),
                Style::default().fg(p.text_dim),
            ),
        ]));

        frame.render_widget(Paragraph::new(lines).block(block), area);
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

    #[test]
    fn estimate_value_thresholds() {
        assert_eq!(estimate_value(100), "high");
        assert_eq!(estimate_value(80), "high");
        assert_eq!(estimate_value(79), "medium");
        assert_eq!(estimate_value(60), "medium");
        assert_eq!(estimate_value(59), "low");
        assert_eq!(estimate_value(0), "low");
    }

    #[test]
    fn blended_cost_applies_session_rate_to_change_tokens() {
        // Session: $1.00 over 1000 tokens → $0.001/token. Change used 100 tokens
        // → $0.10 = 100_000 microdollars.
        assert_eq!(blended_cost_micros(1.0, 1000, 100), 100_000);
        // Whole session attributed to the change → full session cost.
        assert_eq!(blended_cost_micros(2.5, 500, 500), 2_500_000);
    }

    #[test]
    fn blended_cost_guards_missing_data() {
        assert_eq!(blended_cost_micros(0.0, 1000, 100), 0, "no cost yet");
        assert_eq!(blended_cost_micros(1.0, 0, 100), 0, "no tokens yet");
        assert_eq!(blended_cost_micros(1.0, 1000, 0), 0, "change has no tokens");
    }
}
