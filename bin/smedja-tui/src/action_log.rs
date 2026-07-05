//! `ActionLog` widget — ring buffer of last 50 audit events, collapsible.

use std::collections::VecDeque;

use crate::theme::palette;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// A single entry in the action log.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// HH:MM:SS formatted timestamp.
    pub timestamp: String,
    /// Action type or verb (e.g. "bash", "`read_file`").
    pub action: String,
    /// Tool name (may be empty).
    pub tool_name: String,
    /// Outcome ("ok", "error", "blocked").
    pub outcome: String,
}

/// Ring-buffer widget showing the last N audit events.
#[derive(Debug)]
pub struct ActionLog {
    events: VecDeque<AuditEntry>,
    max: usize,
    /// When false, the widget renders as a single collapsed line.
    pub visible: bool,
}

impl ActionLog {
    /// Creates a new [`ActionLog`] with the given ring-buffer capacity.
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            events: VecDeque::new(),
            max,
            visible: true,
        }
    }

    /// Pushes an entry, evicting the oldest if capacity is exceeded.
    pub fn push(&mut self, entry: AuditEntry) {
        if self.events.len() >= self.max {
            self.events.pop_front();
        }
        self.events.push_back(entry);
    }

    /// Number of retained events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Returns `true` when no events are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Renders the action log into `frame` at `area`.
    pub fn render(&self, area: Rect, frame: &mut Frame) {
        if !self.visible {
            let collapsed = Paragraph::new("[ action log hidden — press Shift-A to show ]")
                .block(Block::default().borders(Borders::ALL).title("actions"));
            frame.render_widget(collapsed, area);
            return;
        }

        let p = palette();
        let lines: Vec<Line> = self
            .events
            .iter()
            .map(|e| {
                let outcome_color = match e.outcome.as_str() {
                    "ok" => p.success,
                    "error" => p.error,
                    "sys" => p.accent,
                    _ => p.warn,
                };
                Line::from(vec![
                    Span::raw(format!("{} ", e.timestamp)),
                    Span::styled(e.action.clone(), Style::default()),
                    Span::raw(if e.tool_name.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", e.tool_name)
                    }),
                    Span::raw(" \u{2192} "),
                    Span::styled(e.outcome.clone(), Style::default().fg(outcome_color)),
                ])
            })
            .collect();

        let title = if self.events.len() >= self.max {
            format!("actions ({}/{})", self.events.len(), self.max)
        } else {
            format!("actions ({})", self.events.len())
        };
        let widget =
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(widget, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_evicts_oldest_at_capacity() {
        let mut log = ActionLog::new(50);
        for i in 0..=50 {
            log.push(AuditEntry {
                timestamp: format!("00:00:{i:02}"),
                action: "bash".into(),
                tool_name: String::new(),
                outcome: "ok".into(),
            });
        }
        // 51 pushes into a 50-cap buffer should keep only 50.
        assert_eq!(log.len(), 50);
    }

    #[test]
    fn oldest_entry_evicted() {
        let mut log = ActionLog::new(2);
        log.push(AuditEntry {
            timestamp: "00:00:01".into(),
            action: "first".into(),
            tool_name: String::new(),
            outcome: "ok".into(),
        });
        log.push(AuditEntry {
            timestamp: "00:00:02".into(),
            action: "second".into(),
            tool_name: String::new(),
            outcome: "ok".into(),
        });
        log.push(AuditEntry {
            timestamp: "00:00:03".into(),
            action: "third".into(),
            tool_name: String::new(),
            outcome: "ok".into(),
        });
        // "first" should have been evicted.
        assert!(log.events.front().map(|e| e.action.as_str()) != Some("first"));
        assert_eq!(log.events.back().map(|e| e.action.as_str()), Some("third"));
    }

    #[test]
    fn visible_flag_toggles() {
        let mut log = ActionLog::new(50);
        assert!(log.visible, "starts visible");
        // Mirrors the Shift-A keybinding handler: `visible = !visible`.
        log.visible = !log.visible;
        assert!(!log.visible, "toggled to hidden");
        log.visible = !log.visible;
        assert!(log.visible, "toggled back to visible");
    }
}
