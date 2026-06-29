//! LSP panel widget — compact diagnostic summary for the right-hand rail.
//!
//! Renders below the context token bar when `Ctrl-L` is active. Shows one
//! status line per running language server followed by the highest-severity
//! diagnostics that fit in the available height.

use crate::theme::palette;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use smedja_lsp::{LspSnapshot, ServerState, Severity};

pub struct LspPanel<'a> {
    pub snapshot: &'a LspSnapshot,
    /// Code-graph symbol count from this session's last `/index`, shown as a
    /// footer line so graph status sits directly under the LSP panel.
    graph_symbols: Option<usize>,
}

impl<'a> LspPanel<'a> {
    #[must_use]
    pub fn new(snapshot: &'a LspSnapshot) -> Self {
        Self {
            snapshot,
            graph_symbols: None,
        }
    }

    /// Attaches the code-graph symbol count (from `/index`) to the footer.
    #[must_use]
    pub fn with_graph(mut self, graph_symbols: Option<usize>) -> Self {
        self.graph_symbols = graph_symbols;
        self
    }

    pub fn render(&self, area: Rect, frame: &mut Frame) {
        if area.height < 2 {
            return;
        }

        let p = palette();
        let snap = self.snapshot;
        let inner_h = area.height.saturating_sub(2) as usize; // subtract borders
        let w = area.width.saturating_sub(2) as usize;

        let mut lines: Vec<Line<'_>> = Vec::new();

        // ── Empty state ──────────────────────────────────────────────────────
        if snap.servers.is_empty() {
            lines.push(Line::from(Span::styled(
                "no LSP servers detected",
                Style::default().fg(p.text_dim),
            )));
            lines.push(Line::from(Span::styled(
                "install a language server for your project",
                Style::default().fg(p.text_dim),
            )));
        }

        // ── Server status lines ──────────────────────────────────────────────
        for server in &snap.servers {
            if lines.len() >= inner_h {
                break;
            }
            let (dot, color) = match &server.state {
                ServerState::Starting => ("\u{25cc}", p.warn),     // ◌
                ServerState::Ready => ("\u{25cf}", p.success),     // ●
                ServerState::Degraded(_) => ("\u{2717}", p.error), // ✗
            };
            let name_max = w.saturating_sub(2); // dot + space
            let name = trunc_str(&server.name, name_max);
            lines.push(Line::from(vec![
                Span::styled(dot, Style::default().fg(color)),
                Span::raw(format!(" {name}")),
            ]));
        }

        // ── Diagnostic lines ─────────────────────────────────────────────────
        // Leave one row for the summary footer.
        let diag_rows = inner_h.saturating_sub(snap.servers.len()).saturating_sub(1);

        for diag in snap.diagnostics.iter().take(diag_rows) {
            let (label, color) = match diag.severity {
                Severity::Error => ("E", p.error),
                Severity::Warning => ("W", p.warn),
                Severity::Info => ("I", p.accent),
                Severity::Hint => ("H", p.text_dim),
            };
            let file_name = diag
                .file
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?");
            // "E file.rs:42" — truncate file name to fit
            let loc = format!("{}:{}", file_name, diag.line);
            let loc_max = w.saturating_sub(2); // severity + space
            let loc_str = if loc.len() > loc_max {
                format!("\u{2026}{}", &loc[loc.len().saturating_sub(loc_max - 1)..])
            } else {
                loc
            };
            lines.push(Line::from(vec![
                Span::styled(label, Style::default().fg(color)),
                Span::raw(format!(" {loc_str}")),
            ]));
        }

        // ── Footer summary ───────────────────────────────────────────────────
        let errors = snap.error_count();
        let warnings = snap.warning_count();
        if errors > 0 || warnings > 0 {
            lines.push(Line::from(vec![
                Span::styled(format!("{errors}E"), Style::default().fg(p.error)),
                Span::raw(" "),
                Span::styled(format!("{warnings}W"), Style::default().fg(p.warn)),
            ]));
        } else if !snap.servers.is_empty() && snap.diagnostics.is_empty() {
            lines.push(Line::from(Span::styled(
                "clean",
                Style::default().fg(p.success),
            )));
        }

        // ── Code-graph footer ───────────────────────────────────────────────
        if lines.len() < inner_h {
            let graph = match self.graph_symbols {
                Some(n) => Span::styled(
                    format!("\u{2317} graph: {n} symbols"),
                    Style::default().fg(p.accent),
                ),
                None => Span::styled(
                    "\u{2317} graph: /index to build",
                    Style::default().fg(p.text_dim),
                ),
            };
            lines.push(Line::from(graph));
        }

        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(p.border))
                    .title(" lsp "),
            ),
            area,
        );
    }
}

fn trunc_str(s: &str, max: usize) -> &str {
    &s[..s.char_indices().nth(max).map_or(s.len(), |(i, _)| i)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_lsp::{Diagnostic, LspSnapshot, ServerState, ServerStatus, Severity};
    use std::path::PathBuf;

    fn diag(severity: Severity, file: &str, line: u32, msg: &str) -> Diagnostic {
        Diagnostic {
            file: PathBuf::from(file),
            line,
            col: 1,
            severity,
            code: None,
            message: msg.to_owned(),
        }
    }

    #[test]
    fn error_count_matches_severity() {
        let snap = LspSnapshot {
            servers: vec![],
            diagnostics: vec![
                diag(Severity::Error, "main.rs", 1, "err"),
                diag(Severity::Warning, "lib.rs", 2, "warn"),
                diag(Severity::Error, "util.rs", 3, "err2"),
            ],
        };
        assert_eq!(snap.error_count(), 2);
        assert_eq!(snap.warning_count(), 1);
    }

    #[test]
    fn panel_renders_server_status() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let snap = LspSnapshot {
            servers: vec![ServerStatus {
                name: "rust-analyzer".to_owned(),
                state: ServerState::Ready,
            }],
            diagnostics: vec![diag(Severity::Error, "src/main.rs", 42, "E0308")],
        };
        let panel = LspPanel::new(&snap);
        let backend = TestBackend::new(27, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| panel.render(f.area(), f)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(rendered.contains("rust-analyzer") || rendered.contains("rust-anal"));
    }
}
