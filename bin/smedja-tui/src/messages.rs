use crate::action_log;
use crate::main_panel;
use crate::state::{AppState, Message, Role};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Appends a single operational entry to the actions log (the "emit" rail),
/// timestamped, without touching the message panel.
pub(crate) fn push_action_log(state: &mut AppState, action: impl Into<String>) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let ts = format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    );
    state.action_log.push(action_log::AuditEntry {
        timestamp: ts,
        action: action.into(),
        tool_name: String::new(),
        outcome: "sys".to_owned(),
    });
}

pub(crate) fn push_system_message(state: &mut AppState, text: impl Into<String>) {
    let msg = Message {
        role: Role::System,
        text: text.into(),
    };
    // Short single-line operational messages are also routed to the action log
    // (the "emit" rail in the SuperConsole pattern) so they appear in both
    // the main panel and the scrolling event strip.
    let first_line = msg.text.lines().next().unwrap_or("").to_owned();
    if !msg.text.contains('\n') {
        let ts = {
            use std::time::{SystemTime, UNIX_EPOCH};
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let h = (secs / 3600) % 24;
            let m = (secs / 60) % 60;
            let s = secs % 60;
            format!("{h:02}:{m:02}:{s:02}")
        };
        state.action_log.push(action_log::AuditEntry {
            timestamp: ts,
            action: first_line,
            tool_name: String::new(),
            outcome: "sys".to_owned(),
        });
    }
    state.main_panel.push_line(msg.text.clone());
    state.messages.push(msg);
}

/// Formats a tool call's full arguments into overlay lines, pretty-printing the
/// JSON input when possible. Used by right-click expansion and `/tools`.
pub(crate) fn format_tool_detail(name: &str, full: &str) -> Vec<String> {
    let mut lines = vec![format!("tool: {name}"), String::new()];
    let pretty = serde_json::from_str::<serde_json::Value>(full)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| full.to_owned());
    lines.extend(pretty.lines().map(str::to_owned));
    lines.push(String::new());
    lines.push("(Esc to close)".to_owned());
    lines
}

/// Builds an author chip line (`▌ you` / `▌ claude`) marking a turn boundary so
/// messages have clear authorship. Pushed on its own line; the message body
/// follows beneath it.
/// Pushes an author chip, preceded by a blank spacer line (a turn separator)
/// when the panel already has content — so successive turns read as distinct
/// blocks instead of one running mass of text.
pub(crate) fn push_author_chip(
    panel: &mut main_panel::MainPanel,
    label: &str,
    color: Color,
    no_color: bool,
) {
    if !panel.is_empty() {
        let blanks = panel.spacing.blank_rows_after_chip();
        // Compact mode inserts no spacer; default (Comfortable) inserts one.
        let blanks = blanks.max(1);
        for _ in 0..blanks {
            panel.push_styled_line(Line::from(""));
        }
    }
    panel.push_styled_line(author_chip(label, color, no_color));
}

pub(crate) fn author_chip(label: &str, color: Color, no_color: bool) -> Line<'static> {
    let style = if no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    };
    Line::from(Span::styled(format!("▌ {label}"), style))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::testutil::{make_state, render_frame};
    #[allow(unused_imports)]
    use crate::tool_call::{tool_call_card, tool_glyph_label};
    #[allow(unused_imports)]
    use serde_json::{json, Value};

    #[test]
    fn tool_glyph_label_compacts_verbose_names() {
        let (g, l) = tool_glyph_label("ToolSearch");
        assert_eq!((g, l.as_str()), ("⌕", "search"));
        let (_, bash) = tool_glyph_label("Bash");
        assert_eq!(bash, "bash");
        // Unknown tool → lowercased, capped.
        let (g2, l2) = tool_glyph_label("SomeReallyLongToolName");
        assert_eq!(g2, "▶");
        assert!(l2.chars().count() <= 14);
    }

    #[test]
    fn format_tool_detail_pretty_prints_json_args() {
        let lines = format_tool_detail("Bash", r#"{"command":"ls -la","timeout":5}"#);
        let joined = lines.join("\n");
        assert!(joined.contains("tool: Bash"), "{joined}");
        assert!(joined.contains("\"command\""), "{joined}"); // pretty JSON
        assert!(joined.contains("ls -la"), "{joined}");
        assert!(joined.contains("Esc to close"), "{joined}");
        // Non-JSON falls back to raw.
        let raw = format_tool_detail("X", "not json");
        assert!(raw.join("\n").contains("not json"));
    }

    #[test]
    fn tool_call_card_shows_glyph_label_and_summary() {
        let line = tool_call_card("Bash", "find . -type f", true, '\u{2713}');
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("bash"), "{text}");
        assert!(text.contains("find . -type f"), "{text}");
        assert!(text.contains('\u{2713}'), "{text}"); // status glyph present
                                                      // No raw JSON braces leak into the card.
        assert!(!text.contains('{'), "{text}");
    }

    #[test]
    fn push_system_message_routes_single_line_to_action_log() {
        let mut state = make_state("sess-emit");
        let log_before = state.action_log.len();
        push_system_message(&mut state, "diagram saved: ./out.svg");
        assert_eq!(
            state.action_log.len(),
            log_before + 1,
            "single-line system message must be added to action_log"
        );
    }

    #[test]
    fn push_system_message_multi_line_stays_in_panel_only() {
        let mut state = make_state("sess-emit-multi");
        let log_before = state.action_log.len();
        push_system_message(&mut state, "line one\nline two\nline three");
        assert_eq!(
            state.action_log.len(),
            log_before,
            "multi-line system message must NOT be added to action_log"
        );
    }
}
