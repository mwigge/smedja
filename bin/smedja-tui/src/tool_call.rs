//! Tool-call card rendering — glyph mapping and styled line builder.

use crate::theme::palette;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Maps a tool name to a glyph + compact display label.
pub(crate) fn tool_glyph_label(name: &str) -> (&'static str, String) {
    match name {
        "Bash" | "bash" | "shell" => ("⌘", "bash".to_owned()),
        "Read" | "read" => ("◇", "read".to_owned()),
        "Write" | "write" => ("✎", "write".to_owned()),
        "Edit" | "edit" | "MultiEdit" | "Update" => ("✎", "edit".to_owned()),
        "Grep" | "grep" | "search_files" => ("⌕", "grep".to_owned()),
        "Glob" | "glob" | "find" => ("⌕", "glob".to_owned()),
        "ToolSearch" => ("⌕", "search".to_owned()),
        "WebFetch" | "fetch" => ("⬇", "fetch".to_owned()),
        "WebSearch" => ("⌕", "web".to_owned()),
        "Task" | "Agent" => ("◈", "agent".to_owned()),
        "TodoWrite" => ("☑", "todo".to_owned()),
        "NotebookEdit" => ("✎", "notebook".to_owned()),
        other => {
            let s: String = other.to_lowercase().chars().take(14).collect();
            ("▶", s)
        }
    }
}

/// Builds a one-line tool-call card: `<status> <glyph> <label>  <summary>`.
///
/// `status` is the progress glyph: a spinner frame while running, `✓` on
/// success, `✗` on error.  Glyph + label are accented/bold; summary is dimmed.
pub(crate) fn tool_call_card(
    name: &str,
    input: &str,
    no_color: bool,
    status: char,
) -> Line<'static> {
    let (glyph, label) = tool_glyph_label(name);
    let (status_style, head_style, arg_style) = if no_color {
        (
            Style::default(),
            Style::default().add_modifier(Modifier::BOLD),
            Style::default(),
        )
    } else {
        let p = palette();
        let st = match status {
            '\u{2713}' => Style::default().fg(p.code_added), // ✓
            '\u{2717}' => Style::default().fg(p.code_removed), // ✗
            _ => Style::default().fg(p.text_dim),
        };
        (
            st,
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            Style::default().fg(p.text_dim),
        )
    };
    let mut spans = vec![
        Span::styled(format!("{status} "), status_style),
        Span::styled(format!("{glyph} {label}"), head_style),
    ];
    if !input.is_empty() {
        spans.push(Span::styled(format!("  {input}"), arg_style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_glyph_label_compacts_verbose_names() {
        let (g, l) = tool_glyph_label("ToolSearch");
        assert_eq!(g, "⌕");
        assert_eq!(l, "search");
        let (_, bash) = tool_glyph_label("Bash");
        assert_eq!(bash, "bash");
        let (g2, l2) = tool_glyph_label("SomeReallyLongToolName");
        assert_eq!(g2, "▶");
        assert_eq!(l2.len(), 14);
    }

    #[test]
    fn tool_call_card_shows_glyph_label_and_summary() {
        let line = tool_call_card("Bash", "find . -type f", true, '\u{2713}');
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("bash"));
        assert!(text.contains("find . -type f"));
    }

    #[test]
    fn tool_call_card_first_span_carries_spinner_frame() {
        // A running card is built with a braille spinner frame as the status char;
        // it must appear in the leading status span.
        let line = tool_call_card("Read", "src/main.rs", true, '\u{2819}');
        let first = line.spans.first().expect("status span present");
        assert!(
            first.content.contains('\u{2819}'),
            "first span must carry the spinner frame; got: {:?}",
            first.content
        );
    }
}
