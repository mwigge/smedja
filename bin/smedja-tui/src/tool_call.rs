//! Tool-call card rendering — ACP `ToolKind` glyphs, status pills, and the
//! collapsible one-line/expanded card used in the transcript.
//!
//! A completed tool collapses to a single skimmable line
//! (`⏵ execute cargo test … ✔ 4.2s`); a failed tool keeps a red-bordered,
//! expanded body. MCP tools carry an `mcp:` tag.

use crate::theme::palette;
use crate::viz::{pill, PillKind};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Agent-Client-Protocol tool kinds — the atomic classification every tool call
/// is bucketed into for its glyph and colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// Reading a file / resource.
    Read,
    /// Editing / writing a file.
    Edit,
    /// Deleting a resource.
    Delete,
    /// Moving / renaming.
    Move,
    /// Searching (grep/glob/web).
    Search,
    /// Executing a command / shell.
    Execute,
    /// Reasoning / sub-agent thought.
    Think,
    /// Fetching a remote resource.
    Fetch,
    /// Anything else.
    Other,
}

impl ToolKind {
    /// The ACP kind glyph.
    #[must_use]
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Read => "◲",
            Self::Edit => "✎",
            Self::Delete => "␡",
            Self::Move => "⇄",
            Self::Search => "⌕",
            Self::Execute => "⏵",
            Self::Think => "✻",
            Self::Fetch => "⇩",
            Self::Other => "•",
        }
    }

    /// The short kind label shown in the collapsed line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Delete => "delete",
            Self::Move => "move",
            Self::Search => "search",
            Self::Execute => "execute",
            Self::Think => "think",
            Self::Fetch => "fetch",
            Self::Other => "tool",
        }
    }
}

/// Classifies a tool name into its ACP [`ToolKind`]. Handles native tool names,
/// lowercase variants, and MCP-namespaced names (`mcp__server__do_thing`).
#[must_use]
pub fn tool_kind_of(name: &str) -> ToolKind {
    // Strip an MCP namespace so `mcp__github__search_issues` classifies on the
    // trailing action verb.
    let bare = name.rsplit("__").next().unwrap_or(name);
    let lower = bare.to_ascii_lowercase();
    let l = lower.as_str();
    if matches!(l, "bash" | "shell" | "execute" | "run" | "exec") {
        return ToolKind::Execute;
    }
    if matches!(l, "read" | "cat" | "open" | "notebookread") {
        return ToolKind::Read;
    }
    if matches!(
        l,
        "write" | "edit" | "multiedit" | "update" | "notebookedit" | "create" | "apply_patch"
    ) {
        return ToolKind::Edit;
    }
    if matches!(l, "delete" | "rm" | "remove") {
        return ToolKind::Delete;
    }
    if matches!(l, "move" | "rename" | "mv") {
        return ToolKind::Move;
    }
    if matches!(
        l,
        "grep" | "glob" | "find" | "search" | "search_files" | "toolsearch" | "websearch" | "web"
    ) {
        return ToolKind::Search;
    }
    if matches!(l, "webfetch" | "fetch" | "download" | "get") {
        return ToolKind::Fetch;
    }
    if matches!(l, "task" | "agent" | "think" | "reason") {
        return ToolKind::Think;
    }
    // Substring heuristics for MCP verbs that don't match exactly.
    if l.contains("search") || l.contains("list") || l.contains("query") {
        ToolKind::Search
    } else if l.contains("read") || l.contains("get") {
        ToolKind::Read
    } else if l.contains("write") || l.contains("edit") || l.contains("create") {
        ToolKind::Edit
    } else if l.contains("fetch") || l.contains("download") {
        ToolKind::Fetch
    } else {
        ToolKind::Other
    }
}

/// Whether `name` is an MCP-namespaced tool (`mcp__server__tool` or `mcp:tool`).
#[must_use]
pub fn is_mcp(name: &str) -> bool {
    name.starts_with("mcp__") || name.starts_with("mcp:")
}

/// Formats an elapsed wall-clock in the compact `4.2s` / `820ms` / `1.5m` form.
#[must_use]
pub fn fmt_elapsed(secs: f32) -> String {
    if secs >= 60.0 {
        format!("{:.1}m", secs / 60.0)
    } else if secs >= 1.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}ms", (secs * 1000.0).round() as i64)
    }
}

/// The resolved status of a tool card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardStatus {
    /// Still running — carries the current spinner frame.
    Running(char),
    /// Completed successfully.
    Ok,
    /// Failed.
    Failed,
}

impl CardStatus {
    /// Derives a status from the legacy single-char status marker: `✓`/`✗`,
    /// anything else is treated as a running spinner frame.
    #[must_use]
    pub fn from_char(c: char) -> Self {
        match c {
            '\u{2713}' => Self::Ok,
            '\u{2717}' => Self::Failed,
            other => Self::Running(other),
        }
    }
}

/// Builds the collapsed one-line tool card:
/// `<status> <kind-glyph> <kind-label>  <mcp:> <title>  <✔ elapsed>`.
///
/// Running cards lead with a molten spinner; completed cards trail their glyph
/// and elapsed; failed cards use the error colour. The card is width-agnostic
/// (rendered left-aligned into the line-based transcript).
#[must_use]
pub fn tool_card_line(
    name: &str,
    input: &str,
    no_color: bool,
    status: CardStatus,
    elapsed_s: Option<f32>,
) -> Line<'static> {
    let kind = tool_kind_of(name);
    let p = palette();

    let (lead, head_style, arg_style) = if no_color {
        let lead = match status {
            CardStatus::Running(f) => f.to_string(),
            CardStatus::Ok => "\u{2713}".to_owned(),
            CardStatus::Failed => "\u{2717}".to_owned(),
        };
        (
            lead,
            Style::default().add_modifier(Modifier::BOLD),
            Style::default(),
        )
    } else {
        let (lead, lead_color) = match status {
            CardStatus::Running(f) => (f.to_string(), p.molten),
            CardStatus::Ok => ("\u{2713}".to_owned(), p.success),
            CardStatus::Failed => ("\u{2717}".to_owned(), p.error),
        };
        // Failed heads glow red so the eye lands on them.
        let head_color = if status == CardStatus::Failed {
            p.error
        } else {
            p.accent
        };
        let _ = lead_color;
        (
            lead,
            Style::default().fg(head_color).add_modifier(Modifier::BOLD),
            Style::default().fg(p.text_dim),
        )
    };

    let lead_style = if no_color {
        Style::default()
    } else {
        let c = match status {
            CardStatus::Running(_) => p.molten,
            CardStatus::Ok => p.success,
            CardStatus::Failed => p.error,
        };
        Style::default().fg(c).add_modifier(Modifier::BOLD)
    };

    let mut spans = vec![
        Span::styled(format!("{lead} "), lead_style),
        Span::styled(format!("{} {}", kind.glyph(), kind.label()), head_style),
    ];

    if is_mcp(name) {
        let mcp_style = if no_color {
            Style::default()
        } else {
            Style::default().fg(p.local).add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled("  mcp:", mcp_style));
    }

    if !input.is_empty() {
        let title: String = input.chars().take(72).collect();
        let ellipsis = if input.chars().count() > 72 {
            "…"
        } else {
            ""
        };
        spans.push(Span::styled(format!("  {title}{ellipsis}"), arg_style));
    }

    // Trailing elapsed for settled cards.
    if !matches!(status, CardStatus::Running(_)) {
        if let Some(secs) = elapsed_s {
            let el_style = if no_color {
                Style::default()
            } else {
                Style::default().fg(p.text_dim)
            };
            spans.push(Span::styled(format!("  {}", fmt_elapsed(secs)), el_style));
        }
    }

    Line::from(spans)
}

/// Backwards-compatible single-line card builder (no elapsed).
///
/// `status` is the legacy progress char: a spinner frame while running, `✓` on
/// success, `✗` on error.
pub(crate) fn tool_call_card(
    name: &str,
    input: &str,
    no_color: bool,
    status: char,
) -> Line<'static> {
    tool_card_line(name, input, no_color, CardStatus::from_char(status), None)
}

/// Builds the width-aware header row of an expanded card with a right-aligned
/// status pill. Used where a full card (known width) is drawn rather than the
/// collapsed transcript line.
#[must_use]
pub fn card_header(
    name: &str,
    title: &str,
    width: usize,
    no_color: bool,
    status: CardStatus,
) -> Line<'static> {
    let kind = tool_kind_of(name);
    let p = palette();
    let pk = match status {
        CardStatus::Running(_) => PillKind::Running,
        CardStatus::Ok => PillKind::Done,
        CardStatus::Failed => PillKind::Fail,
    };
    let pill_span = pill(pk, no_color);
    let pill_w = pill_span.content.chars().count();

    let head = format!("{} {}", kind.glyph(), kind.label());
    let mcp = if is_mcp(name) { "mcp: " } else { "" };
    let avail = width.saturating_sub(head.chars().count() + pill_w + 3 + mcp.chars().count());
    let title_short: String = title.chars().take(avail.max(1)).collect();
    let left = format!("{head}  {mcp}{title_short}");
    let pad = width.saturating_sub(left.chars().count() + pill_w).max(1);

    let head_style = if no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
    };
    Line::from(vec![
        Span::styled(left, head_style),
        Span::raw(" ".repeat(pad)),
        pill_span,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_kind_maps_native_names() {
        assert_eq!(tool_kind_of("Bash"), ToolKind::Execute);
        assert_eq!(tool_kind_of("Read"), ToolKind::Read);
        assert_eq!(tool_kind_of("Edit"), ToolKind::Edit);
        assert_eq!(tool_kind_of("MultiEdit"), ToolKind::Edit);
        assert_eq!(tool_kind_of("Grep"), ToolKind::Search);
        assert_eq!(tool_kind_of("WebFetch"), ToolKind::Fetch);
        assert_eq!(tool_kind_of("Task"), ToolKind::Think);
    }

    #[test]
    fn tool_kind_maps_mcp_namespaced() {
        assert_eq!(tool_kind_of("mcp__github__search_issues"), ToolKind::Search);
        assert_eq!(
            tool_kind_of("mcp__drive__read_file_content"),
            ToolKind::Read
        );
        assert_eq!(tool_kind_of("mcp__gmail__create_draft"), ToolKind::Edit);
    }

    #[test]
    fn every_kind_has_glyph_and_label() {
        for k in [
            ToolKind::Read,
            ToolKind::Edit,
            ToolKind::Delete,
            ToolKind::Move,
            ToolKind::Search,
            ToolKind::Execute,
            ToolKind::Think,
            ToolKind::Fetch,
            ToolKind::Other,
        ] {
            assert!(!k.glyph().is_empty());
            assert!(!k.label().is_empty());
        }
        assert_eq!(ToolKind::Execute.glyph(), "⏵");
        assert_eq!(ToolKind::Read.glyph(), "◲");
    }

    #[test]
    fn is_mcp_detects_namespace() {
        assert!(is_mcp("mcp__github__x"));
        assert!(is_mcp("mcp:do"));
        assert!(!is_mcp("Bash"));
    }

    #[test]
    fn fmt_elapsed_scales() {
        assert_eq!(fmt_elapsed(0.42), "420ms");
        assert_eq!(fmt_elapsed(4.2), "4.2s");
        assert_eq!(fmt_elapsed(90.0), "1.5m");
    }

    #[test]
    fn card_status_from_char() {
        assert_eq!(CardStatus::from_char('\u{2713}'), CardStatus::Ok);
        assert_eq!(CardStatus::from_char('\u{2717}'), CardStatus::Failed);
        assert!(matches!(
            CardStatus::from_char('\u{2819}'),
            CardStatus::Running('\u{2819}')
        ));
    }

    #[test]
    fn collapsed_line_shows_kind_and_elapsed() {
        let line = tool_card_line("Bash", "cargo test", true, CardStatus::Ok, Some(4.2));
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("execute"), "kind label: {text}");
        assert!(text.contains("cargo test"), "title: {text}");
        assert!(text.contains("4.2s"), "elapsed: {text}");
        assert!(text.contains('\u{2713}'), "ok glyph: {text}");
    }

    #[test]
    fn running_line_leads_with_spinner_and_no_elapsed() {
        let line = tool_card_line(
            "Read",
            "src/main.rs",
            true,
            CardStatus::Running('\u{2819}'),
            Some(9.9),
        );
        let first = line.spans.first().unwrap();
        assert!(first.content.contains('\u{2819}'));
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        // Running cards never show elapsed.
        assert!(!text.contains("9.9s"), "no elapsed while running: {text}");
    }

    #[test]
    fn mcp_line_carries_tag() {
        let line = tool_card_line(
            "mcp__github__list_issues",
            "repo:x",
            true,
            CardStatus::Ok,
            None,
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("mcp:"), "mcp tag present: {text}");
    }

    #[test]
    fn tool_call_card_backward_compatible() {
        let line = tool_call_card("Bash", "find . -type f", true, '\u{2713}');
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("execute"));
        assert!(text.contains("find . -type f"));
    }

    #[test]
    fn tool_call_card_first_span_carries_spinner_frame() {
        let line = tool_call_card("Read", "src/main.rs", true, '\u{2819}');
        let first = line.spans.first().expect("status span present");
        assert!(first.content.contains('\u{2819}'));
    }

    #[test]
    fn card_header_right_aligns_pill_within_width() {
        let line = card_header("Bash", "cargo test --all", 40, true, CardStatus::Ok);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("execute"));
        assert!(text.contains("DONE"));
        assert!(text.chars().count() <= 42, "roughly within width: {text}");
    }
}
