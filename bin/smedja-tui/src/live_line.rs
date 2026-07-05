//! The live line — a dedicated bottom row shown while a turn is active.
//!
//! `⟨spinner⟩ ⟨state-verb⟩ · ⟨elapsed⟩ · ⟨moving counter⟩ · [esc] cancel`
//!
//! Two state-keyed spinner sets tell the mode apart at a glance:
//! - a **calm braille** cycle while THINKING / streaming, and
//! - an **active molten star** cycle while a tool is RUNNING.
//!
//! The line is always paired with a moving number (streamed token count while
//! thinking; tool wall-clock while running) so the "is it frozen?" test is a
//! ticking timer *and* a changing count. If output stalls past
//! [`STALL_SECS`], the line degrades to a dim `⚠ no output Ns`.

use crate::theme::palette;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Calm braille spinner for THINKING / streaming.
const BRAILLE: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];
/// Active molten star cycle for RUNNING-TOOL.
const STARS: [char; 6] = ['·', '✻', '✽', '✶', '✳', '✢'];

/// Seconds without new output before the line degrades to a stall warning.
pub const STALL_SECS: u64 = 8;

/// Which mode the turn is in — selects the spinner set and colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveState {
    /// Model is thinking / streaming text.
    Thinking,
    /// A tool is executing.
    RunningTool,
}

impl LiveState {
    /// The spinner glyph for this state at `tick`.
    #[must_use]
    pub fn spinner(self, tick: u8) -> char {
        match self {
            Self::Thinking => BRAILLE[tick as usize % BRAILLE.len()],
            Self::RunningTool => STARS[tick as usize % STARS.len()],
        }
    }
}

/// Formats a running elapsed as `4.2s` / `1.5m`.
#[must_use]
pub fn fmt_secs(secs: f32) -> String {
    if secs >= 60.0 {
        format!("{:.1}m", secs / 60.0)
    } else {
        format!("{secs:.1}s")
    }
}

/// Builds the live line. `verb` is the state verb (e.g. `thinking`, `running
/// bash`); `counter` is the moving field label (e.g. `1.2k tok`, `4.2s`);
/// `stalled_secs` is seconds since the last output — when it exceeds
/// [`STALL_SECS`] the line collapses to a stall warning.
#[must_use]
pub fn live_line(
    state: LiveState,
    verb: &str,
    elapsed_s: f32,
    counter: &str,
    stalled_secs: u64,
    tick: u8,
    no_color: bool,
) -> Line<'static> {
    let p = palette();

    if stalled_secs >= STALL_SECS {
        let warn_style = if no_color {
            Style::default()
        } else {
            Style::default().fg(p.warn).add_modifier(Modifier::DIM)
        };
        return Line::from(vec![Span::styled(
            format!("⚠ no output {stalled_secs}s · [esc] cancel"),
            warn_style,
        )]);
    }

    let spin = state.spinner(tick);
    let (spin_style, verb_style, sep_style) = if no_color {
        (Style::default(), Style::default(), Style::default())
    } else {
        let spin_color = match state {
            LiveState::Thinking => p.accent,
            LiveState::RunningTool => p.molten,
        };
        (
            Style::default().fg(spin_color).add_modifier(Modifier::BOLD),
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            Style::default().fg(p.text_dim),
        )
    };
    let num_style = if no_color {
        Style::default()
    } else {
        Style::default().fg(p.accent)
    };

    Line::from(vec![
        Span::styled(format!("{spin} "), spin_style),
        Span::styled(verb.to_owned(), verb_style),
        Span::styled(" · ", sep_style),
        Span::styled(fmt_secs(elapsed_s), num_style),
        Span::styled(" · ", sep_style),
        Span::styled(counter.to_owned(), num_style),
        Span::styled(" · [esc] cancel", sep_style),
    ])
}

/// Renders the live line onto the bottom row of `area`. No-op when the turn is
/// not in flight or the area has no height.
#[allow(clippy::too_many_arguments)]
pub fn render(
    area: Rect,
    turn_in_flight: bool,
    state: LiveState,
    verb: &str,
    elapsed_s: f32,
    counter: &str,
    stalled_secs: u64,
    tick: u8,
    no_color: bool,
    frame: &mut Frame,
) {
    if !turn_in_flight || area.height < 1 {
        return;
    }
    let row = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(1),
        area.width,
        1,
    );
    let line = live_line(
        state,
        verb,
        elapsed_s,
        counter,
        stalled_secs,
        tick,
        no_color,
    );
    frame.render_widget(Paragraph::new(line), row);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn thinking_and_running_use_different_spinner_sets() {
        // Braille for thinking, stars for running — the glyph tells the mode.
        assert!(BRAILLE.contains(&LiveState::Thinking.spinner(0)));
        assert!(STARS.contains(&LiveState::RunningTool.spinner(0)));
        assert_ne!(
            LiveState::Thinking.spinner(1),
            LiveState::RunningTool.spinner(1)
        );
    }

    #[test]
    fn spinner_cycles_and_wraps() {
        assert_eq!(LiveState::Thinking.spinner(0), BRAILLE[0]);
        assert_eq!(
            LiveState::Thinking.spinner(8),
            BRAILLE[0],
            "wraps at set length"
        );
    }

    #[test]
    fn fmt_secs_scales() {
        assert_eq!(fmt_secs(4.2), "4.2s");
        assert_eq!(fmt_secs(90.0), "1.5m");
    }

    #[test]
    fn live_line_pairs_timer_and_counter() {
        let line = live_line(LiveState::Thinking, "thinking", 3.4, "1.2k tok", 0, 0, true);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("thinking"));
        assert!(text.contains("3.4s"), "ticking timer: {text}");
        assert!(text.contains("1.2k tok"), "moving counter: {text}");
        assert!(text.contains("[esc] cancel"));
    }

    #[test]
    fn live_line_degrades_on_stall() {
        let line = live_line(
            LiveState::Thinking,
            "thinking",
            9.0,
            "1.2k tok",
            STALL_SECS,
            0,
            true,
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("no output"), "stall warning: {text}");
        assert!(text.contains("[esc] cancel"));
    }

    #[test]
    fn render_skips_when_not_in_flight() {
        let backend = TestBackend::new(60, 5);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let a = f.area();
            render(
                a,
                false,
                LiveState::Thinking,
                "thinking",
                1.0,
                "0 tok",
                0,
                0,
                true,
                f,
            );
        })
        .unwrap();
        let rendered: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(!rendered.contains("thinking"));
    }

    #[test]
    fn render_draws_when_in_flight() {
        let backend = TestBackend::new(60, 5);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let a = f.area();
            render(
                a,
                true,
                LiveState::RunningTool,
                "running bash",
                2.5,
                "2.5s",
                0,
                3,
                true,
                f,
            );
        })
        .unwrap();
        let rendered: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(rendered.contains("running bash"), "{rendered}");
    }
}
