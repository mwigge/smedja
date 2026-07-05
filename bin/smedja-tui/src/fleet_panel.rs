//! Multi-agent fleet roster — one row per active agent with a stable identity
//! colour, current activity glyph, a micro-progress bar, `step x/y`, and a
//! status pill, plus a fleet summary line.
//!
//! State is keyed by agent id so interleaved output from several agents stays
//! attributable; each agent keeps its deterministic [`agent_color`] everywhere.

use crate::theme::{agent_color, palette};
use crate::tool_call::{tool_kind_of, ToolKind};
use crate::viz::{microbar_mode, pill, PillKind, RenderMode};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

/// Lifecycle status of a single agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// Actively working.
    Running,
    /// Finished successfully.
    Done,
    /// Finished with a failure.
    Failed,
}

impl AgentStatus {
    fn pill_kind(self) -> PillKind {
        match self {
            Self::Running => PillKind::Running,
            Self::Done => PillKind::Done,
            Self::Failed => PillKind::Fail,
        }
    }
}

/// Live state for one agent in the fleet.
#[derive(Debug, Clone)]
pub struct AgentState {
    /// Stable id (keys the roster).
    pub id: String,
    /// Display name (drives the identity colour).
    pub name: String,
    /// The tool kind of the agent's current activity.
    pub kind: ToolKind,
    /// Short activity description (tool + target).
    pub activity: String,
    /// `(current, total)` step counter; `total == 0` means unknown.
    pub step: (u16, u16),
    /// Lifecycle status.
    pub status: AgentStatus,
}

impl AgentState {
    fn progress(&self) -> f64 {
        let (cur, tot) = self.step;
        if tot == 0 {
            match self.status {
                AgentStatus::Done => 1.0,
                _ => 0.0,
            }
        } else {
            f64::from(cur) / f64::from(tot)
        }
    }
}

/// The fleet of agents seen this session, keyed by id.
#[derive(Debug, Clone, Default)]
pub struct FleetState {
    agents: Vec<AgentState>,
}

impl FleetState {
    /// Registers an agent (if new) and returns nothing. Existing agents keep
    /// their state; a re-seen agent is flipped back to `Running`.
    pub fn upsert(&mut self, id: &str, name: &str) {
        if let Some(a) = self.agents.iter_mut().find(|a| a.id == id) {
            a.status = AgentStatus::Running;
        } else {
            self.agents.push(AgentState {
                id: id.to_owned(),
                name: name.to_owned(),
                kind: ToolKind::Think,
                activity: "starting…".to_owned(),
                step: (0, 0),
                status: AgentStatus::Running,
            });
        }
    }

    /// Updates an agent's current activity from a tool name + target.
    pub fn set_activity(&mut self, id: &str, tool: &str, target: &str) {
        if let Some(a) = self.agents.iter_mut().find(|a| a.id == id) {
            a.kind = tool_kind_of(tool);
            let t: String = target.chars().take(40).collect();
            a.activity = t;
            a.status = AgentStatus::Running;
        }
    }

    /// Advances an agent's `step x/y` counter.
    pub fn set_step(&mut self, id: &str, cur: u16, total: u16) {
        if let Some(a) = self.agents.iter_mut().find(|a| a.id == id) {
            a.step = (cur, total);
        }
    }

    /// Marks an agent done or failed.
    pub fn set_status(&mut self, id: &str, status: AgentStatus) {
        if let Some(a) = self.agents.iter_mut().find(|a| a.id == id) {
            a.status = status;
        }
    }

    /// Colour carried into an agent's transcript cards so interleaved output is
    /// attributable. Returns `None` for an unknown id.
    #[must_use]
    pub fn color_for(&self, id: &str) -> Option<ratatui::style::Color> {
        self.agents
            .iter()
            .find(|a| a.id == id)
            .map(|a| agent_color(&a.name))
    }

    /// Number of registered agents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Whether the fleet is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// `(running, done, failed)` counts.
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut running = 0;
        let mut done = 0;
        let mut failed = 0;
        for a in &self.agents {
            match a.status {
                AgentStatus::Running => running += 1,
                AgentStatus::Done => done += 1,
                AgentStatus::Failed => failed += 1,
            }
        }
        (running, done, failed)
    }
}

/// Builds the fleet summary line: `agents 3 · running 2 · done 1 · failed 0`,
/// with the counts coloured.
#[must_use]
pub fn summary_line(fleet: &FleetState, no_color: bool) -> Line<'static> {
    let p = palette();
    let (running, done, failed) = fleet.counts();
    let dim = if no_color {
        Style::default()
    } else {
        Style::default().fg(p.text_dim)
    };
    let col = |c: ratatui::style::Color| {
        if no_color {
            Style::default()
        } else {
            Style::default().fg(c)
        }
    };
    Line::from(vec![
        Span::styled(format!("agents {} · ", fleet.len()), dim),
        Span::styled(format!("running {running}"), col(p.molten)),
        Span::styled(" · ", dim),
        Span::styled(format!("done {done}"), col(p.success)),
        Span::styled(" · ", dim),
        Span::styled(
            format!("failed {failed}"),
            col(if failed > 0 { p.error } else { p.text_dim }),
        ),
    ])
}

/// Builds one roster row for an agent.
#[must_use]
fn agent_row(a: &AgentState, width: usize, no_color: bool, mode: RenderMode) -> Line<'static> {
    let p = palette();
    let color = agent_color(&a.name);
    let pip_style = if no_color {
        Style::default()
    } else {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    };
    let name_style = if no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(p.text_bright)
            .add_modifier(Modifier::BOLD)
    };
    let dim = if no_color {
        Style::default()
    } else {
        Style::default().fg(p.text_dim)
    };

    let name: String = a.name.chars().take(10).collect();
    let bar = microbar_mode(a.progress(), 1.0, 6, mode);
    let step = if a.step.1 == 0 {
        String::new()
    } else {
        format!(" {}/{}", a.step.0, a.step.1)
    };
    let activity: String = a
        .activity
        .chars()
        .take(width.saturating_sub(28).max(4))
        .collect();

    Line::from(vec![
        Span::styled("◆ ", pip_style),
        Span::styled(format!("{name:<10} "), name_style),
        Span::styled(format!("{} ", a.kind.glyph()), Style::default().fg(color)),
        Span::styled(format!("{activity} "), dim),
        Span::styled(bar, Style::default().fg(color)),
        Span::styled(step, dim),
        Span::raw(" "),
        pill(a.status.pill_kind(), no_color),
    ])
}

/// The fleet roster panel.
pub struct FleetPanel<'a> {
    /// Fleet to render.
    pub fleet: &'a FleetState,
    /// Terminal glyph mode.
    pub mode: RenderMode,
    /// Disable colour.
    pub no_color: bool,
}

impl FleetPanel<'_> {
    /// Renders the roster inside a bordered ` fleet ` block.
    pub fn render(&self, area: Rect, frame: &mut Frame) {
        if area.height < 3 {
            return;
        }
        let p = palette();
        let inner_w = (area.width as usize).saturating_sub(2).max(1);
        let mut lines: Vec<Line<'static>> = vec![summary_line(self.fleet, self.no_color)];
        for a in &self.fleet.agents {
            lines.push(agent_row(a, inner_w, self.no_color, self.mode));
        }
        let border_style = if self.no_color {
            Style::default()
        } else {
            Style::default().fg(p.border)
        };
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(border_style)
                    .title(" fleet [Ctrl-G] "),
            ),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fleet3() -> FleetState {
        let mut f = FleetState::default();
        f.upsert("a1", "planner");
        f.upsert("a2", "implementer");
        f.upsert("a3", "reviewer");
        f.set_activity("a2", "Bash", "cargo test");
        f.set_step("a2", 3, 5);
        f.set_status("a3", AgentStatus::Done);
        f
    }

    #[test]
    fn upsert_registers_and_dedups() {
        let mut f = FleetState::default();
        f.upsert("a1", "planner");
        f.upsert("a1", "planner");
        assert_eq!(f.len(), 1);
        f.upsert("a2", "impl");
        assert_eq!(f.len(), 2);
    }

    #[test]
    fn counts_partition_by_status() {
        let f = fleet3();
        // a1,a2 running; a3 done; 0 failed.
        assert_eq!(f.counts(), (2, 1, 0));
    }

    #[test]
    fn color_stable_per_agent_name() {
        let f = fleet3();
        assert_eq!(f.color_for("a1"), Some(agent_color("planner")));
        assert!(f.color_for("missing").is_none());
    }

    #[test]
    fn progress_from_step_and_done() {
        let f = fleet3();
        let a2 = f.agents.iter().find(|a| a.id == "a2").unwrap();
        assert!((a2.progress() - 0.6).abs() < 1e-9);
        let a3 = f.agents.iter().find(|a| a.id == "a3").unwrap();
        assert!((a3.progress() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn summary_line_lists_counts() {
        let text: String = summary_line(&fleet3(), true)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("agents 3"));
        assert!(text.contains("running 2"));
        assert!(text.contains("done 1"));
        assert!(text.contains("failed 0"));
    }

    #[test]
    fn agent_row_carries_name_step_and_activity() {
        let f = fleet3();
        let a2 = f.agents.iter().find(|a| a.id == "a2").unwrap();
        let text: String = agent_row(a2, 60, true, RenderMode::Block)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("implement"), "name: {text}");
        assert!(text.contains("cargo test"), "activity: {text}");
        assert!(text.contains("3/5"), "step: {text}");
    }

    #[test]
    fn panel_renders_without_panic() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let f = fleet3();
        let panel = FleetPanel {
            fleet: &f,
            mode: RenderMode::Braille,
            no_color: false,
        };
        let backend = TestBackend::new(48, 8);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f2| panel.render(f2.area(), f2)).unwrap();
        let rendered: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(rendered.contains("fleet"));
    }
}
