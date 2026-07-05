//! In-terminal OTel trace waterfall — the current turn's spans (the turn span
//! plus its tool-call children) laid out as indented horizontal duration bars
//! on a shared time axis, otel-tui style.
//!
//! Spans are sourced from the tool-call events' start/end timings already in the
//! stream, plus the enclosing turn span — no full OTLP pipeline, just a layout
//! of the turn's known spans. A keybind expands a span's detail.

use crate::theme::palette;
use crate::tool_call::{tool_kind_of, ToolKind};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// Status of a span, driving its bar colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanStatus {
    /// Still open.
    Running,
    /// Closed successfully.
    Ok,
    /// Closed with an error.
    Failed,
}

/// One span in the turn trace.
#[derive(Debug, Clone)]
pub struct TraceSpan {
    /// Display name (`turn`, tool name, …).
    pub name: String,
    /// Milliseconds from turn start when the span opened.
    pub start_ms: u64,
    /// Duration in milliseconds (0 while still running — laid out to `now`).
    pub dur_ms: u64,
    /// Indentation depth (0 = the turn root, 1 = its tool children).
    pub depth: u8,
    /// Span status.
    pub status: SpanStatus,
}

impl TraceSpan {
    fn end_ms(&self) -> u64 {
        self.start_ms.saturating_add(self.dur_ms)
    }

    fn kind(&self) -> ToolKind {
        tool_kind_of(&self.name)
    }
}

/// The current/selected turn's trace: a root turn span and its tool children.
#[derive(Debug, Clone, Default)]
pub struct TurnTrace {
    /// All spans, root first, in start order.
    pub spans: Vec<TraceSpan>,
}

impl TurnTrace {
    /// Opens a fresh turn root span at t=0.
    pub fn start_turn(&mut self) {
        self.spans.clear();
        self.spans.push(TraceSpan {
            name: "turn".to_owned(),
            start_ms: 0,
            dur_ms: 0,
            depth: 0,
            status: SpanStatus::Running,
        });
    }

    /// Pushes a running tool child span starting at `start_ms`.
    pub fn push_tool(&mut self, name: impl Into<String>, start_ms: u64) {
        self.spans.push(TraceSpan {
            name: name.into(),
            start_ms,
            dur_ms: 0,
            depth: 1,
            status: SpanStatus::Running,
        });
    }

    /// Settles the most recent still-running tool span, closing it at `end_ms`.
    pub fn settle_last_tool(&mut self, end_ms: u64, ok: bool) {
        if let Some(span) = self
            .spans
            .iter_mut()
            .rev()
            .find(|s| s.depth == 1 && s.status == SpanStatus::Running)
        {
            span.dur_ms = end_ms.saturating_sub(span.start_ms);
            span.status = if ok {
                SpanStatus::Ok
            } else {
                SpanStatus::Failed
            };
        }
    }

    /// Closes the turn root at `total_ms` and settles any tool still open.
    pub fn finish(&mut self, total_ms: u64, ok: bool) {
        for span in &mut self.spans {
            if span.status == SpanStatus::Running {
                if span.depth == 0 {
                    span.dur_ms = total_ms;
                } else {
                    span.dur_ms = total_ms.saturating_sub(span.start_ms);
                }
                span.status = if ok {
                    SpanStatus::Ok
                } else {
                    SpanStatus::Failed
                };
            }
        }
    }

    /// Whether there is anything to draw beyond an empty root.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Total wall-clock the axis spans (max span end, floored at 1ms).
    #[must_use]
    pub fn total_ms(&self) -> u64 {
        self.spans
            .iter()
            .map(TraceSpan::end_ms)
            .max()
            .unwrap_or(0)
            .max(1)
    }
}

fn status_color(status: SpanStatus, kind: ToolKind) -> Color {
    let p = palette();
    match status {
        SpanStatus::Failed => p.error,
        SpanStatus::Running => p.molten,
        SpanStatus::Ok => match kind {
            ToolKind::Execute => p.accent,
            ToolKind::Read | ToolKind::Fetch | ToolKind::Search => p.local,
            ToolKind::Edit | ToolKind::Delete | ToolKind::Move => p.warn,
            ToolKind::Think => p.deep,
            ToolKind::Other => p.success,
        },
    }
}

/// Renders a positioned duration bar into an `axis_w`-wide track: leading
/// spaces to the span's start offset, then a run of `█` for its duration
/// (minimum one cell so zero-duration spans stay visible), space-padded to the
/// full width.
fn bar_track(start_ms: u64, dur_ms: u64, total_ms: u64, axis_w: usize) -> (usize, String) {
    if axis_w == 0 {
        return (0, String::new());
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let offset = ((start_ms as f64 / total_ms as f64) * axis_w as f64).floor() as usize;
    let offset = offset.min(axis_w.saturating_sub(1));
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let len = ((dur_ms as f64 / total_ms as f64) * axis_w as f64).round() as usize;
    let len = len.clamp(1, axis_w - offset);
    let bar = "█".repeat(len);
    (offset, bar)
}

/// Lays the trace out as one line per span: `indent name  [────bar────]  Nms`.
/// `selected` bolds the chosen row for the detail keybind.
#[must_use]
pub fn waterfall_lines(
    trace: &TurnTrace,
    width: usize,
    no_color: bool,
    selected: Option<usize>,
) -> Vec<Line<'static>> {
    if trace.is_empty() || width < 12 {
        return Vec::new();
    }
    let p = palette();
    let total = trace.total_ms();
    // Column budget: name label | axis track | duration label.
    let name_w = (width / 3).clamp(8, 22);
    let dur_w = 7usize;
    let axis_w = width.saturating_sub(name_w + dur_w + 2).max(4);

    trace
        .spans
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let indent = usize::from(s.depth) * 2;
            let avail = name_w.saturating_sub(indent).max(1);
            let mut label: String = s.name.chars().take(avail.saturating_sub(1)).collect();
            if s.name.chars().count() > avail.saturating_sub(1) {
                label.push('…');
            }
            let label = format!("{}{label}", " ".repeat(indent));
            let label_padded = format!("{label:<name_w$}");

            let (offset, bar) = bar_track(s.start_ms, s.dur_ms, total, axis_w);
            let color = status_color(s.status, s.kind());
            let is_sel = selected == Some(i);

            let label_style = if no_color {
                if is_sel {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                }
            } else if is_sel {
                Style::default()
                    .fg(p.text_bright)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.text)
            };
            let bar_style = if no_color {
                Style::default()
            } else {
                Style::default().fg(color)
            };
            let dur_style = if no_color {
                Style::default()
            } else {
                Style::default().fg(p.text_dim)
            };

            let dur_label = format!("{:>dur_w$}", format!("{}ms", s.dur_ms));
            Line::from(vec![
                Span::styled(label_padded, label_style),
                Span::raw(" ".repeat(offset)),
                Span::styled(bar, bar_style),
                Span::raw(" ".repeat(axis_w.saturating_sub(offset + bar_len(s, total, axis_w)))),
                Span::styled(format!(" {dur_label}"), dur_style),
            ])
        })
        .collect()
}

// Recomputes bar length for trailing-pad calculation (kept in sync with
// `bar_track`).
fn bar_len(s: &TraceSpan, total: u64, axis_w: usize) -> usize {
    bar_track(s.start_ms, s.dur_ms, total, axis_w)
        .1
        .chars()
        .count()
}

/// Detail lines for the selected span (shown when a span is expanded).
#[must_use]
pub fn span_detail_lines(trace: &TurnTrace, selected: usize, no_color: bool) -> Vec<Line<'static>> {
    let Some(s) = trace.spans.get(selected) else {
        return Vec::new();
    };
    let p = palette();
    let key = |t: &str| {
        if no_color {
            Span::raw(format!("{t}: "))
        } else {
            Span::styled(format!("{t}: "), Style::default().fg(p.text_dim))
        }
    };
    let val = |t: String| {
        if no_color {
            Span::raw(t)
        } else {
            Span::styled(t, Style::default().fg(p.text))
        }
    };
    let status = match s.status {
        SpanStatus::Running => "running",
        SpanStatus::Ok => "ok",
        SpanStatus::Failed => "failed",
    };
    vec![
        Line::from(vec![
            key("span"),
            val(s.name.clone()),
            key("  kind"),
            val(s.kind().label().to_owned()),
        ]),
        Line::from(vec![
            key("start"),
            val(format!("+{}ms", s.start_ms)),
            key("  dur"),
            val(format!("{}ms", s.dur_ms)),
        ]),
        Line::from(vec![key("status"), val(status.to_owned())]),
    ]
}

/// Renders the waterfall inside a bordered ` trace ` block.
pub fn render(
    area: Rect,
    frame: &mut Frame,
    trace: &TurnTrace,
    selected: Option<usize>,
    expanded: bool,
    no_color: bool,
) {
    if area.height < 3 || trace.is_empty() {
        return;
    }
    let p = palette();
    let inner_w = (area.width as usize).saturating_sub(2).max(1);
    let lines = waterfall_lines(trace, inner_w, no_color, selected);
    let border_style = if no_color {
        Style::default()
    } else {
        Style::default().fg(p.border)
    };
    // Keep the hint accurate to what `x` actually does in the current state:
    // open the inspector when collapsed, step to the next span when expanded.
    let title = if expanded {
        " trace [x: next span] "
    } else {
        " trace [x: inspect] "
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(title),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TurnTrace {
        let mut t = TurnTrace::default();
        t.start_turn();
        t.push_tool("Read", 100);
        t.settle_last_tool(300, true);
        t.push_tool("Bash", 350);
        t.settle_last_tool(1350, true);
        t.finish(1400, true);
        t
    }

    #[test]
    fn trace_accumulates_root_and_children() {
        let t = sample();
        assert_eq!(t.spans.len(), 3);
        assert_eq!(t.spans[0].name, "turn");
        assert_eq!(t.spans[0].depth, 0);
        assert_eq!(t.spans[1].depth, 1);
        assert_eq!(t.spans[0].dur_ms, 1400);
        assert_eq!(t.spans[1].dur_ms, 200); // 300-100
        assert_eq!(t.spans[2].dur_ms, 1000); // 1350-350
    }

    #[test]
    fn total_ms_is_max_span_end() {
        assert_eq!(sample().total_ms(), 1400);
    }

    #[test]
    fn finish_settles_open_tool() {
        let mut t = TurnTrace::default();
        t.start_turn();
        t.push_tool("Bash", 100);
        // never settled explicitly
        t.finish(500, true);
        assert_eq!(t.spans[1].status, SpanStatus::Ok);
        assert_eq!(t.spans[1].dur_ms, 400);
    }

    #[test]
    fn failed_tool_marked_failed() {
        let mut t = TurnTrace::default();
        t.start_turn();
        t.push_tool("Bash", 0);
        t.settle_last_tool(50, false);
        assert_eq!(t.spans[1].status, SpanStatus::Failed);
    }

    #[test]
    fn bar_track_positions_and_sizes() {
        // A span from 50%..100% of a 10-wide axis → offset 5, len 5.
        let (off, bar) = bar_track(500, 500, 1000, 10);
        assert_eq!(off, 5);
        assert_eq!(bar.chars().count(), 5);
    }

    #[test]
    fn bar_track_zero_duration_stays_visible() {
        let (_off, bar) = bar_track(0, 0, 1000, 10);
        assert_eq!(bar.chars().count(), 1, "min one cell");
    }

    #[test]
    fn waterfall_lines_one_per_span_and_fits_width() {
        let t = sample();
        let lines = waterfall_lines(&t, 60, true, Some(1));
        assert_eq!(lines.len(), 3);
        for l in &lines {
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(w <= 62, "line within width: {w}");
        }
    }

    #[test]
    fn waterfall_empty_for_tiny_width() {
        assert!(waterfall_lines(&sample(), 8, true, None).is_empty());
    }

    #[test]
    fn span_detail_lists_timing() {
        let t = sample();
        let lines = span_detail_lines(&t, 1, true);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("Read"));
        assert!(text.contains("+100ms"));
        assert!(text.contains("200ms"));
    }

    #[test]
    fn render_no_panic() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let t = sample();
        let backend = TestBackend::new(50, 8);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f.area(), f, &t, Some(0), false, false))
            .unwrap();
        let rendered: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(rendered.contains("trace"));
    }
}
