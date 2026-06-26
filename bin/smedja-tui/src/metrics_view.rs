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
use ratatui::style::{Modifier, Style};
use crate::theme::palette;
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

/// One per-source savings row for the latest window.
#[derive(Debug, Clone, PartialEq)]
pub struct SavingsRow {
    /// Saving source (`filter`, `crusher`, `cold-context`, `cache`, …).
    pub source: String,
    /// Total tokens saved by this source in the window.
    pub tokens_saved: i64,
}

/// The token-economy savings snapshot: per-source rows plus the headline split.
///
/// Cache savings are "input not re-paid" and are kept as a separate figure from
/// the compression total (`filter` + `crusher` + `cold-context`); they are never
/// folded into one number.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SavingsSnapshot {
    /// Per-source savings rows, in display order.
    pub rows: Vec<SavingsRow>,
    /// Sum of compression savings (`filter` + `crusher` + `cold-context`).
    pub compression_saved: i64,
    /// Sum of cache savings (`source = 'cache'`).
    pub cache_saved: i64,
    /// Efficiency ratio `saved / (saved + billed_input)` over the window.
    pub efficiency_ratio: f64,
}

/// Maps a `savings.summary` RPC JSON response into a [`SavingsSnapshot`].
///
/// Pure: no I/O. Unknown or missing fields default to empty / zero so a partial
/// or stale response renders an empty section rather than panicking.
#[must_use]
pub fn savings_snapshot_from_json(resp: &serde_json::Value) -> SavingsSnapshot {
    let rows = resp["buckets"]
        .as_array()
        .map(|buckets| {
            buckets
                .iter()
                .map(|b| SavingsRow {
                    source: b["source"].as_str().unwrap_or("-").to_owned(),
                    tokens_saved: b["tokens_saved"].as_i64().unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default();
    SavingsSnapshot {
        rows,
        compression_saved: resp["compression_saved"].as_i64().unwrap_or(0),
        cache_saved: resp["cache_saved"].as_i64().unwrap_or(0),
        efficiency_ratio: resp["efficiency_ratio"].as_f64().unwrap_or(0.0),
    }
}

/// The metrics view widget — renders a cached per-runner snapshot plus the
/// token-economy savings section.
pub struct MetricsView {
    /// Per-runner rows for the latest window, in display order.
    pub rows: Vec<MetricsRow>,
    /// Token-economy savings snapshot for the latest window.
    pub savings: SavingsSnapshot,
}

impl MetricsView {
    /// Width of the panel when shown as a right-hand sidebar.
    pub const WIDTH: u16 = 38;

    /// Wraps `rows` and a savings `snapshot` for rendering.
    #[must_use]
    pub fn with_savings(rows: Vec<MetricsRow>, savings: SavingsSnapshot) -> Self {
        Self { rows, savings }
    }

    /// Renders the panel content as text lines (header + one row per runner, or
    /// a placeholder when there is no data, followed by the savings section).
    /// Exposed for unit testing the layout without a `Buffer`.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!(
            "{:<10} {:>8} {:>9} {:>5}",
            "RUNNER", "TOKENS", "COST", "ERR"
        )];
        if self.rows.is_empty() {
            out.push("(no metrics)".to_owned());
        } else {
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
        }
        out.extend(self.savings_lines());
        out
    }

    /// Renders the savings section: an efficiency headline, the compression and
    /// cache totals as separate figures, and one row per source. When there is no
    /// savings data the section shows an empty placeholder (not a stale value).
    fn savings_lines(&self) -> Vec<String> {
        let s = &self.savings;
        let mut out = vec![
            String::new(),
            format!("SAVINGS  eff {:.0}%", s.efficiency_ratio * 100.0),
            format!("  compression {:>10} tok", s.compression_saved),
            format!("  cache       {:>10} tok", s.cache_saved),
        ];
        if s.rows.is_empty() {
            out.push("  (no savings)".to_owned());
            return out;
        }
        for row in &s.rows {
            let source = &row.source[..row.source.len().min(12)];
            out.push(format!("  {:<12} {:>10}", source, row.tokens_saved));
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
                        Style::default().fg(palette().error)
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
        let view = MetricsView::with_savings(vec![], SavingsSnapshot::default());
        let lines = view.lines();
        let joined = lines.join("\n");
        assert!(lines[0].contains("RUNNER") && lines[0].contains("ERR"));
        assert!(joined.contains("(no metrics)"));
        // The savings section shows its own empty placeholder, not a stale value.
        assert!(joined.contains("(no savings)"));
    }

    #[test]
    fn lines_render_per_runner_cost_tokens_and_errors() {
        let view = MetricsView::with_savings(
            vec![
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
            ],
            SavingsSnapshot::default(),
        );
        let lines = view.lines();
        assert!(lines[1].contains("claude"));
        assert!(lines[1].contains("780"));
        assert!(lines[1].contains("$0.0600"));
        assert!(lines[1].contains('1'), "claude error count shown");
        assert!(lines[2].contains("local"));
        assert!(lines[2].contains("$0.0000"));
    }

    #[test]
    fn savings_snapshot_from_json_maps_rows_and_split() {
        let resp = serde_json::json!({
            "tier": "daily",
            "compression_saved": 150,
            "cache_saved": 9000,
            "efficiency_ratio": 0.25,
            "buckets": [
                { "bucket_start": 0, "source": "cache", "tokens_saved": 9000 },
                { "bucket_start": 0, "source": "filter", "tokens_saved": 150 },
            ],
        });
        let snap = savings_snapshot_from_json(&resp);
        assert_eq!(snap.compression_saved, 150);
        assert_eq!(snap.cache_saved, 9000);
        assert!((snap.efficiency_ratio - 0.25).abs() < 1e-9);
        assert_eq!(snap.rows.len(), 2);
        assert_eq!(snap.rows[0].source, "cache");
        assert_eq!(snap.rows[0].tokens_saved, 9000);
    }

    #[test]
    fn savings_section_keeps_compression_and_cache_separate() {
        let snap = SavingsSnapshot {
            rows: vec![
                SavingsRow {
                    source: "filter".into(),
                    tokens_saved: 150,
                },
                SavingsRow {
                    source: "cache".into(),
                    tokens_saved: 9000,
                },
            ],
            compression_saved: 150,
            cache_saved: 9000,
            efficiency_ratio: 0.25,
        };
        let view = MetricsView::with_savings(vec![], snap);
        let joined = view.lines().join("\n");
        assert!(joined.contains("eff 25%"), "efficiency headline: {joined}");
        assert!(joined.contains("compression"), "compression figure shown");
        assert!(joined.contains("cache"), "cache figure shown");
        // Compression and cache are separate figures, never summed to 9150.
        assert!(!joined.contains("9150"));
    }

    #[test]
    fn savings_section_empty_when_no_data() {
        let view = MetricsView::with_savings(
            vec![MetricsRow {
                runner: "claude".into(),
                tokens: 10,
                cost_usd: 0.0,
                errors: 0,
            }],
            SavingsSnapshot::default(),
        );
        let joined = view.lines().join("\n");
        assert!(joined.contains("(no savings)"));
    }

    #[test]
    fn renders_into_buffer_without_panicking() {
        let view = MetricsView::with_savings(
            vec![MetricsRow {
                runner: "claude".into(),
                tokens: 100,
                cost_usd: 0.01,
                errors: 0,
            }],
            SavingsSnapshot::default(),
        );
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
