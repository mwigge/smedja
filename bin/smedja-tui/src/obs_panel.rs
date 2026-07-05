//! Observability panel widget — p95/p99 latency, token throughput, context
//! fill, daily quota, session cost, and cache efficiency in the right-hand rail.
//!
//! Renders below the LSP panel when `Ctrl-O` is active. All data is taken from
//! `ObsSnapshot`; the widget never fetches or blocks.

use std::collections::VecDeque;

use crate::theme::palette;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// Immutable snapshot of all observability data the panel needs to render.
#[derive(Debug, Clone, Default)]
pub struct ObsSnapshot {
    /// Round-trip turn latencies in milliseconds (capped at 50 entries).
    pub latency_samples: VecDeque<u64>,
    /// Cumulative input tokens for this session.
    pub tokens_input: u64,
    /// Cumulative output tokens for this session.
    pub tokens_output: u64,
    /// Context window size in tokens for the active model.
    pub context_window: u64,
    /// Context tokens used so far.
    pub context_used: u64,
    /// Estimated session cost in USD.
    pub session_cost_usd: f64,
    /// `saved / (saved + billed)` over the current savings window.
    pub efficiency_ratio: f64,
    /// Tokens saved by cache in the current window.
    pub cache_saved: i64,
    /// Daily tokens consumed (`None` when unknown).
    pub daily_tokens_used: Option<u64>,
    /// Daily token budget (`None` when unknown or unlimited).
    pub daily_tokens_limit: Option<u64>,
}

impl ObsSnapshot {
    fn p95_ms(&self) -> Option<u64> {
        percentile(&self.latency_samples, 95)
    }

    fn p99_ms(&self) -> Option<u64> {
        percentile(&self.latency_samples, 99)
    }
}

fn percentile(samples: &VecDeque<u64>, pct: usize) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted: Vec<u64> = samples.iter().copied().collect();
    sorted.sort_unstable();
    // Index clamped to the last valid position.
    let idx = ((pct * sorted.len()) / 100)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    Some(sorted[idx])
}

#[allow(clippy::cast_precision_loss)]
fn fmt_ms(ms: u64) -> String {
    if ms >= 60_000 {
        format!("{:.1}m", ms as f64 / 60_000.0)
    } else if ms >= 1_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{ms}ms")
    }
}

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

/// Eighth-block glyphs for the latency sparkline, low → high.
const SPARK_BLOCKS: [char; 8] = [
    '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}',
];

/// Renders the most recent `width` latency samples as an inline block
/// sparkline, scaled to the window's own min/max. Returns an empty string for
/// an empty series or zero width.
fn sparkline(samples: &VecDeque<u64>, width: usize) -> String {
    if width == 0 || samples.is_empty() {
        return String::new();
    }
    let recent: Vec<u64> = samples.iter().rev().take(width).rev().copied().collect();
    let max = recent.iter().copied().max().unwrap_or(0);
    let min = recent.iter().copied().min().unwrap_or(0);
    let span = max.saturating_sub(min).max(1);
    recent
        .iter()
        .map(|&v| {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let idx = (((v - min) as f64 / span as f64) * 7.0).round() as usize;
            SPARK_BLOCKS[idx.min(7)]
        })
        .collect()
}

/// Shared budget thresholds: green below 60%, amber 60–85%, red at/above 85%.
/// Delegates to the single [`crate::viz::zone_color`] helper so every gauge in
/// the TUI shares one green/amber/red rule.
fn budget_color(pct: u64) -> ratatui::style::Color {
    #[allow(clippy::cast_possible_truncation)]
    crate::viz::zone_color(pct.min(100) as u8)
}

/// Builds a width-aware labeled gauge line: `prefix [████░░░] suffix`. The bar
/// is `value/max` coloured by [`budget_color`] and shrinks to fit `width` after
/// the prefix/suffix so the numeric labels are never truncated.
fn labeled_gauge(prefix: &str, suffix: &str, value: u64, max: u64, width: usize) -> Line<'static> {
    let p = palette();
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let pct = if max == 0 {
        0
    } else {
        ((value as f64 / max as f64) * 100.0).min(100.0) as u64
    };
    let reserved = prefix.chars().count() + suffix.chars().count();
    let bar_w = width.saturating_sub(reserved).max(1);
    let bar = fill_bar(value, max, bar_w, budget_color(pct));
    let mut spans = vec![Span::styled(
        prefix.to_owned(),
        Style::default().fg(p.text_dim),
    )];
    spans.extend(bar.spans);
    spans.push(Span::styled(
        suffix.to_owned(),
        Style::default().fg(p.text_dim),
    ));
    Line::from(spans)
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn fill_bar(value: u64, max: u64, width: usize, color: ratatui::style::Color) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let filled = if max == 0 {
        0usize
    } else {
        ((value as f64 / max as f64) * width as f64)
            .round()
            .min(width as f64) as usize
    };
    let empty = width - filled;
    Line::from(vec![
        Span::styled("\u{2588}".repeat(filled), Style::default().fg(color)),
        Span::styled(
            "\u{2591}".repeat(empty),
            Style::default().fg(palette().text_dim),
        ),
    ])
}

/// The observability rail panel.
pub struct ObsPanel<'a> {
    pub snapshot: &'a ObsSnapshot,
}

impl<'a> ObsPanel<'a> {
    #[must_use]
    pub fn new(snapshot: &'a ObsSnapshot) -> Self {
        Self { snapshot }
    }

    #[allow(
        clippy::too_many_lines,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    pub fn render(&self, area: Rect, frame: &mut Frame) {
        if area.height < 3 {
            return;
        }

        let p = palette();
        let snap = self.snapshot;
        let bar_w = (area.width as usize).saturating_sub(6).max(1);
        let mut lines: Vec<Line<'_>> = Vec::new();

        // ── Latency p95 / p99 ───────────────────────────────────────────────
        match (snap.p95_ms(), snap.p99_ms()) {
            (Some(p95), Some(p99)) => {
                lines.push(Line::from(vec![
                    Span::styled("p95", Style::default().fg(p.text_dim)),
                    Span::raw(format!(" {:>5}  ", fmt_ms(p95))),
                    Span::styled("p99", Style::default().fg(p.text_dim)),
                    Span::raw(format!(" {}", fmt_ms(p99))),
                ]));
            }
            _ => {
                lines.push(Line::from(Span::styled(
                    "p95  \u{2014}   p99  \u{2014}",
                    Style::default().fg(p.text_dim),
                )));
            }
        }

        // ── Latency trend sparkline ──────────────────────────────────────────
        let spark = sparkline(&snap.latency_samples, bar_w);
        if !spark.is_empty() {
            lines.push(Line::from(Span::styled(
                spark,
                Style::default().fg(p.local),
            )));
        }

        // ── Token throughput ─────────────────────────────────────────────────
        let total = snap.tokens_input + snap.tokens_output;
        lines.push(Line::from(vec![
            Span::styled("tok ", Style::default().fg(p.text_dim)),
            Span::styled(
                fmt_tok(total),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("  \u{2191}", Style::default().fg(p.success)),
            Span::raw(fmt_tok(snap.tokens_input)),
            Span::styled(" \u{2193}", Style::default().fg(p.warn)),
            Span::raw(fmt_tok(snap.tokens_output)),
        ]));

        // ── Context (token-budget) gauge: green/amber/red by fill ────────────
        if snap.context_window > 0 {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let pct = ((snap.context_used as f64 / snap.context_window as f64) * 100.0) as u64;
            let suffix = format!(" {} {pct}%", fmt_tok(snap.context_used));
            lines.push(labeled_gauge(
                "ctx ",
                &suffix,
                snap.context_used,
                snap.context_window,
                bar_w,
            ));
        }

        // ── Daily quota gauge ────────────────────────────────────────────────
        if let (Some(used), Some(limit)) = (snap.daily_tokens_used, snap.daily_tokens_limit) {
            if limit > 0 {
                #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
                let pct = ((used as f64 / limit as f64) * 100.0).min(100.0) as u64;
                let suffix = format!(" {pct}%");
                lines.push(labeled_gauge("day ", &suffix, used, limit, bar_w));
            }
        }

        // ── Session cost ─────────────────────────────────────────────────────
        if snap.session_cost_usd > 0.0 {
            lines.push(Line::from(vec![
                Span::styled("\u{24}", Style::default().fg(p.text_dim)),
                Span::raw(format!("{:.3}", snap.session_cost_usd)),
            ]));
        }

        // ── Cache-efficiency bar (higher is better → success-toned) ──────────
        if snap.efficiency_ratio > 0.0 {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let eff_pct = (snap.efficiency_ratio * 100.0).round().min(100.0) as u64;
            let suffix = format!(" eff {eff_pct}%");
            let reserved = suffix.chars().count();
            let ew = bar_w.saturating_sub(reserved).max(1);
            let mut spans = fill_bar(eff_pct, 100, ew, p.success).spans;
            spans.push(Span::styled(suffix, Style::default().fg(p.text_dim)));
            lines.push(Line::from(spans));
        }

        // ── Cache savings ────────────────────────────────────────────────────
        if snap.cache_saved > 0 {
            lines.push(Line::from(vec![
                Span::styled("cache ", Style::default().fg(p.text_dim)),
                Span::styled(
                    fmt_tok(snap.cache_saved.cast_unsigned()),
                    Style::default().fg(p.success),
                ),
                Span::raw(" saved"),
            ]));
        }

        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(p.border))
                    .title(" obs "),
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

    #[test]
    fn percentile_empty_returns_none() {
        let empty: VecDeque<u64> = VecDeque::new();
        assert!(percentile(&empty, 95).is_none());
    }

    #[test]
    fn percentile_single_sample() {
        let single: VecDeque<u64> = vec![1500].into();
        assert_eq!(percentile(&single, 95), Some(1500));
        assert_eq!(percentile(&single, 99), Some(1500));
    }

    #[test]
    fn percentile_picks_correct_index() {
        // 10 samples sorted: 100,200,...,1000
        let samples: VecDeque<u64> = (1..=10).map(|i| i * 100).collect();
        // p50 of 10 → idx = (50*10/100)-1 = 4 → value 500
        assert_eq!(percentile(&samples, 50), Some(500));
        // p90 → idx = (90*10/100)-1 = 8 → value 900
        assert_eq!(percentile(&samples, 90), Some(900));
    }

    #[test]
    fn fmt_ms_rounds_correctly() {
        assert_eq!(fmt_ms(500), "500ms");
        assert_eq!(fmt_ms(1500), "1.5s");
        assert_eq!(fmt_ms(90_000), "1.5m");
    }

    #[test]
    fn budget_color_thresholds_green_amber_red() {
        // Green below 60, amber 60–84, red at/above 85.
        assert_eq!(budget_color(0), palette().success);
        assert_eq!(budget_color(59), palette().success);
        assert_eq!(budget_color(60), palette().warn);
        assert_eq!(budget_color(84), palette().warn);
        assert_eq!(budget_color(85), palette().error);
        assert_eq!(budget_color(100), palette().error);
    }

    #[test]
    fn sparkline_maps_range_to_blocks_and_respects_width() {
        let samples: VecDeque<u64> = vec![0, 50, 100].into();
        let s = sparkline(&samples, 10);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars.len(), 3, "one glyph per sample, within width");
        assert_eq!(chars[0], '\u{2581}', "min → lowest block");
        assert_eq!(chars[2], '\u{2588}', "max → highest block");
        // Only the most recent `width` samples are shown.
        let many: VecDeque<u64> = (0..40).collect();
        assert_eq!(sparkline(&many, 8).chars().count(), 8);
        assert!(sparkline(&VecDeque::new(), 8).is_empty());
    }

    #[test]
    fn labeled_gauge_keeps_labels_and_fits_width() {
        // 50% fill, width 20, prefix "ctx " (4) + suffix " 50%" (4) → bar 12.
        let line = labeled_gauge("ctx ", " 50%", 50, 100, 20);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("ctx "), "prefix preserved: {text:?}");
        assert!(text.ends_with(" 50%"), "suffix preserved: {text:?}");
        // Total rendered width never exceeds the budget.
        assert!(text.chars().count() <= 20, "width respected: {text:?}");
    }

    #[test]
    fn panel_renders_without_panic() {
        let snap = ObsSnapshot {
            latency_samples: vec![1000, 2000, 3000, 4000, 5000].into(),
            tokens_input: 50_000,
            tokens_output: 10_000,
            context_window: 200_000,
            context_used: 60_000,
            session_cost_usd: 0.042,
            efficiency_ratio: 0.25,
            cache_saved: 9000,
            ..ObsSnapshot::default()
        };
        let panel = ObsPanel::new(&snap);
        let backend = TestBackend::new(27, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| panel.render(f.area(), f)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(rendered.contains("obs"), "title present: {rendered}");
    }

    #[test]
    fn daily_bar_hidden_when_limit_unknown() {
        let snap = ObsSnapshot {
            daily_tokens_limit: None,
            daily_tokens_used: Some(100),
            ..ObsSnapshot::default()
        };
        let panel = ObsPanel::new(&snap);
        let backend = TestBackend::new(27, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| panel.render(f.area(), f)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            !rendered.contains("dy"),
            "daily bar must be hidden: {rendered}"
        );
    }
}
