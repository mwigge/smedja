//! Inline cowork gate approval widget.
//!
//! Rendered as a centred overlay when a tool call is awaiting human approval.
//! Keyboard shortcuts: `y` approve, `n` deny, `m` modify (edit replacement args
//! as JSON — hidden when the daemon marks the backend `supports_modify: false`),
//! `a` approve-always (persists an allow rule), `Esc` dismiss (deny).

use crate::formatting::wrap_input_rows;
use crate::theme::palette;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};

/// Memoised [`body_rows`] output: `(wrap_width, rows)`.
type RowsCache = std::cell::RefCell<Option<(usize, Vec<(RowKind, String)>)>>;

/// A single pending cowork approval, deserialized from `cowork.pending`.
#[derive(Debug, Clone)]
pub struct CoworkItem {
    /// Approval UUID returned by the daemon.
    pub id: String,
    /// Tool name (e.g. `"bash"`, `"edit_file"`).
    pub tool: String,
    /// Step index within the current turn.
    pub step_n: u32,
    /// Compact string representation of tool arguments.
    pub args_display: String,
    /// Agent's reasoning for invoking this tool.
    pub reasoning: String,
    /// Agent/runner name, when the daemon's wire event carries it.
    pub agent: Option<String>,
    /// Working directory the gated call runs in, when carried.
    pub cwd: Option<String>,
    /// Risk class from the daemon's tool_kind classification, when carried
    /// (`read_only` / `edit` / `exec`).
    pub risk: Option<String>,
    /// Whether the backend can consume a modify decision. From the additive
    /// wire `supports_modify` field; defaults to `true` so older daemons
    /// (which omit it) keep the modify affordance.
    pub supports_modify: bool,
    /// Memoised [`body_rows`] output keyed by wrap width. The body is laid out
    /// twice per frame (once for the overlay height, once for the paint), so
    /// the wrap is computed once per (item, width) and cloned out on hits.
    /// Interior mutability keeps `body_rows(&item, …)` callable through the
    /// shared slice the widget holds.
    pub(crate) rows_cache: RowsCache,
}

impl CoworkItem {
    /// Whether this call is shell/exec-class. Defers to the daemon's wire
    /// `risk` classification when carried (its `tool_kind` table is
    /// authoritative); falls back to the local tool-name table
    /// ([`is_exec_tool`]) for older daemons that omit the field.
    pub(crate) fn is_exec_class(&self) -> bool {
        self.risk
            .as_deref()
            .map_or_else(|| is_exec_tool(&self.tool), |r| r == "exec")
    }
}

/// Section a body row belongs to; drives its styling in the widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowKind {
    /// Agent / cwd / risk meta line.
    Meta,
    /// Extracted shell command (exec-class tools).
    Command,
    /// Full argument text.
    Args,
    /// Agent reasoning.
    Reasoning,
}

/// Tool names whose calls are shell-command executions.
const EXEC_TOOLS: &[&str] = &[
    "bash",
    "shell",
    "exec",
    "sh",
    "cmd",
    "run_command",
    "terminal",
];

/// Returns whether `tool` is a shell/exec-class tool.
pub(crate) fn is_exec_tool(tool: &str) -> bool {
    EXEC_TOOLS.contains(&tool.to_ascii_lowercase().as_str())
}

/// Extracts the shell command from an exec-class tool's args JSON (`cmd` or
/// `command` field). Returns `None` for non-JSON args or args without a
/// command field; the exec-class decision itself belongs to the caller
/// ([`CoworkItem::is_exec_class`], which defers to the wire `risk` class).
fn extract_command_str(args_display: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(args_display).ok()?;
    v.get("cmd")
        .or_else(|| v.get("command"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Hard cap on rows produced by [`body_rows`]: the body is re-wrapped every
/// frame, so an unbounded daemon payload would otherwise burn the render
/// budget. Past the cap the body ends with a single `…` marker.
pub(crate) const BODY_ROWS_CAP: usize = 200;

/// Builds the widget body rows for `item`, wrapped to `width` columns so long
/// commands and args stay fully visible instead of being truncated with `…`.
/// Bounded by [`BODY_ROWS_CAP`] — see the constant. Memoised on the item per
/// wrap width: the render path calls this twice per frame (layout + paint).
pub(crate) fn body_rows(item: &CoworkItem, width: usize) -> Vec<(RowKind, String)> {
    if let Some((cached_width, rows)) = &*item.rows_cache.borrow() {
        if *cached_width == width {
            return rows.clone();
        }
    }
    let rows = compute_body_rows(item, width);
    *item.rows_cache.borrow_mut() = Some((width, rows.clone()));
    rows
}

/// The uncached [`body_rows`] computation.
fn compute_body_rows(item: &CoworkItem, width: usize) -> Vec<(RowKind, String)> {
    let mut rows: Vec<(RowKind, String)> = Vec::new();
    let mut overflowed = false;
    let mut wrap = |kind: RowKind, text: &str| {
        for row in wrap_input_rows(text, width) {
            if rows.len() >= BODY_ROWS_CAP {
                overflowed = true;
                return;
            }
            rows.push((kind, row));
        }
    };

    let meta: Vec<String> = [
        item.agent.as_ref().map(|a| format!("agent: {a}")),
        item.cwd.as_ref().map(|c| format!("cwd: {c}")),
        item.risk.as_ref().map(|r| format!("risk: {r}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !meta.is_empty() {
        wrap(RowKind::Meta, &meta.join(" · "));
    }

    let exec_class = item.is_exec_class();
    let command = if exec_class {
        extract_command_str(&item.args_display)
    } else {
        None
    };
    match command {
        // Exec-class with a parseable command: the command leads on its own
        // lines; the full args stay visible below (dimmed at render).
        Some(cmd) => {
            wrap(RowKind::Command, &format!("$ {cmd}"));
            if !item.args_display.is_empty() {
                wrap(RowKind::Args, &item.args_display);
            }
        }
        None => {
            if !item.args_display.is_empty() {
                let kind = if exec_class {
                    RowKind::Command
                } else {
                    RowKind::Args
                };
                wrap(kind, &item.args_display);
            }
        }
    }

    if !item.reasoning.is_empty() {
        wrap(RowKind::Reasoning, &item.reasoning);
    }
    if overflowed {
        if let Some(last) = rows.last_mut() {
            *last = (RowKind::Args, "\u{2026}".to_owned());
        }
    }
    rows
}

/// The overlay widget's total height (border + body + blank + footer) for
/// `item` rendered at `width` columns.
pub(crate) fn desired_height(item: &CoworkItem, width: u16) -> u16 {
    let inner_width = usize::from(width.saturating_sub(4)).max(1);
    let body = body_rows(item, inner_width).len();
    // 2 border rows + body + 1 spacer + 1 footer.
    u16::try_from(body + 4).unwrap_or(u16::MAX)
}

/// The cowork gate overlay widget.
///
/// Renders the first pending item. Remaining items are shown as a count in the
/// header so the user knows there are more queued behind this one.
pub struct CoworkWidget<'a> {
    pub items: &'a [CoworkItem],
    /// Whether the user has pressed `m` and is editing the replacement args.
    pub modify_mode: bool,
    /// Current content of the modify input (a JSON object of replacement args,
    /// pre-filled from the item's `args_display`).
    pub modify_input: &'a str,
}

impl Widget for CoworkWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let Some(item) = self.items.first() else {
            return;
        };
        let p = palette();

        let queue_suffix = if self.items.len() > 1 {
            format!("  +{} queued", self.items.len() - 1)
        } else {
            String::new()
        };

        let title = format!(
            " COWORK  step {} · {}{} ",
            item.step_n, item.tool, queue_suffix
        );

        let inner_width = usize::from(area.width.saturating_sub(4)).max(1);
        let max_body_rows = usize::from(area.height.saturating_sub(4));
        let mut rows = body_rows(item, inner_width);
        if rows.len() > max_body_rows {
            rows.truncate(max_body_rows.max(1));
            if let Some(last) = rows.last_mut() {
                *last = (RowKind::Args, "\u{2026}".to_owned());
            }
        }

        let mut lines: Vec<Line> = rows
            .into_iter()
            .map(|(kind, text)| {
                let style = match kind {
                    RowKind::Meta | RowKind::Reasoning => Style::default().fg(p.text_dim),
                    RowKind::Command => Style::default().fg(p.text_bright),
                    RowKind::Args => Style::default().fg(p.text),
                };
                Line::from(Span::styled(format!(" {text} "), style))
            })
            .collect();

        let footer_line = if self.modify_mode {
            Line::from(vec![
                Span::raw(" args (JSON): "),
                Span::styled(
                    format!("{}_", self.modify_input),
                    Style::default().fg(p.accent),
                ),
                Span::raw("  "),
                Span::styled("[Esc] cancel", Style::default().fg(p.text_dim)),
            ])
        } else {
            // [m] is only offered when the backend can consume replacement
            // args (wire `supports_modify`); otherwise the daemon would reject
            // the modify with an error anyway.
            let modify_spans: Vec<Span> = if item.supports_modify {
                vec![
                    Span::styled(
                        "[m] ",
                        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("modify  "),
                ]
            } else {
                Vec::new()
            };
            let mut spans = vec![
                Span::styled(
                    " [y] ",
                    Style::default().fg(p.success).add_modifier(Modifier::BOLD),
                ),
                Span::raw("approve  "),
                Span::styled(
                    "[n] ",
                    Style::default().fg(p.error).add_modifier(Modifier::BOLD),
                ),
                Span::raw("deny  "),
            ];
            spans.extend(modify_spans);
            spans.extend([
                Span::styled(
                    "[a] ",
                    Style::default().fg(p.warn).add_modifier(Modifier::BOLD),
                ),
                Span::raw("always  "),
                Span::styled("[Esc] ", Style::default().fg(p.text_dim)),
                Span::styled("dismiss (deny)", Style::default().fg(p.text_dim)),
            ]);
            Line::from(spans)
        };

        lines.push(Line::from(""));
        lines.push(footer_line);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.border))
            .title(Span::styled(
                title,
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            ))
            .title_alignment(Alignment::Left);

        // Clear the area first so the overlay is opaque.
        Clear.render(area, buf);
        Paragraph::new(Text::from(lines))
            .style(Style::default().bg(p.panel))
            .block(block)
            .render(area, buf);
    }
}

/// Computes the overlay rectangle — centred, 80% of `parent` width, tall
/// enough for the item's wrapped body (bounded by `parent`).
#[must_use]
pub fn overlay_rect(parent: Rect, items: &[CoworkItem]) -> Rect {
    // A degenerate parent cannot contain anything — render nothing rather
    // than forcing a 1-cell rect outside its bounds.
    if parent.width == 0 || parent.height == 0 {
        return Rect::new(parent.x, parent.y, 0, 0);
    }
    #[allow(
        clippy::cast_lossless,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let w = ((parent.width as f32) * 0.80) as u16;
    // `u16::clamp` panics when min > max, which a tiny tmux split (parent
    // below the 40×7 minimum) would trigger — fold the parent bound into
    // both ends instead.
    let w = w.clamp(40.min(parent.width), parent.width.max(1));
    let h = items.first().map_or(7, |item| desired_height(item, w));
    let h = h.clamp(7.min(parent.height), parent.height.max(1));
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn item(step_n: u32, tool: &str) -> CoworkItem {
        CoworkItem {
            id: "test-id".into(),
            tool: tool.into(),
            step_n,
            args_display: r#"{"cmd":"ls -la"}"#.into(),
            reasoning: "list files for inspection".into(),
            agent: None,
            cwd: None,
            risk: None,
            supports_modify: true,
            rows_cache: std::cell::RefCell::new(None),
        }
    }

    fn render_widget_at(
        items: &[CoworkItem],
        modify_mode: bool,
        modify_input: &str,
        height: u16,
    ) -> String {
        let backend = TestBackend::new(60, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                let widget = CoworkWidget {
                    items,
                    modify_mode,
                    modify_input,
                };
                widget.render(area, f.buffer_mut());
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut rows = Vec::new();
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .map(|x| {
                    buf.cell((x, y))
                        .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect();
            rows.push(row.trim_end().to_owned());
        }
        rows.join("\n")
    }

    fn render_widget(items: &[CoworkItem], modify_mode: bool, modify_input: &str) -> String {
        render_widget_at(items, modify_mode, modify_input, 12)
    }

    #[test]
    fn widget_renders_tool_name_in_header() {
        let items = vec![item(1, "bash")];
        let output = render_widget(&items, false, "");
        assert!(
            output.contains("bash"),
            "widget must show tool name; got:\n{output}"
        );
    }

    #[test]
    fn widget_renders_all_decision_shortcuts() {
        let items = vec![item(2, "edit_file")];
        let output = render_widget(&items, false, "");
        assert!(output.contains("[y]"), "must show [y] approve");
        assert!(output.contains("[n]"), "must show [n] deny");
        assert!(output.contains("[m]"), "must show [m] modify");
        assert!(output.contains("[a]"), "must show [a] always");
    }

    #[test]
    fn exec_tool_shows_command_on_own_line_plus_full_args() {
        let items = vec![item(1, "bash")];
        let output = render_widget(&items, false, "");
        assert!(
            output.contains("$ ls -la"),
            "command must render on its own line; got:\n{output}"
        );
        assert!(
            output.contains(r#"{"cmd":"ls -la"}"#),
            "full args must stay visible; got:\n{output}"
        );
    }

    #[test]
    fn non_exec_tool_shows_args_without_command_prefix() {
        let mut it = item(1, "edit_file");
        it.args_display = r#"{"path":"src/main.rs"}"#.into();
        let items = vec![it];
        let output = render_widget(&items, false, "");
        assert!(output.contains("src/main.rs"), "args shown; got:\n{output}");
        assert!(
            !output.contains('$'),
            "no command prefix for non-exec tools; got:\n{output}"
        );
    }

    #[test]
    fn meta_line_shows_agent_cwd_risk_when_present() {
        let mut it = item(1, "bash");
        it.agent = Some("review".into());
        it.cwd = Some("/repo".into());
        it.risk = Some("high".into());
        let items = vec![it];
        let output = render_widget(&items, false, "");
        assert!(output.contains("agent: review"), "got:\n{output}");
        assert!(output.contains("cwd: /repo"), "got:\n{output}");
        assert!(output.contains("risk: high"), "got:\n{output}");
    }

    #[test]
    fn long_command_wraps_instead_of_truncating() {
        let mut it = item(1, "bash");
        it.args_display =
            r#"{"cmd":"cargo build --workspace --all-targets --release --verbose"}"#.into();
        it.reasoning = String::new();
        let width = 40usize;
        let rows = body_rows(&it, width);
        let command_rows: Vec<&str> = rows
            .iter()
            .filter(|(k, _)| *k == RowKind::Command)
            .map(|(_, t)| t.as_str())
            .collect();
        assert!(
            command_rows.len() > 1,
            "long command must wrap across rows: {command_rows:?}"
        );
        let joined = command_rows.join("");
        assert!(
            joined.contains("--verbose"),
            "wrapped command must keep its tail: {joined}"
        );
        assert!(
            !joined.contains('\u{2026}'),
            "command must not be truncated with …: {joined}"
        );
    }

    #[test]
    fn modify_mode_shows_json_args_prompt() {
        let items = vec![item(1, "bash")];
        let output = render_widget(&items, true, r#"{"cmd":"ls"}"#);
        assert!(
            output.contains("args (JSON):"),
            "modify mode must label the prompt as JSON args; got:\n{output}"
        );
        assert!(
            output.contains(r#"{"cmd":"ls"}"#),
            "must show current input"
        );
    }

    #[test]
    fn modify_affordance_hidden_when_backend_cannot_modify() {
        let mut it = item(1, "bash");
        it.supports_modify = false;
        let items = vec![it];
        let output = render_widget(&items, false, "");
        assert!(
            !output.contains("[m]"),
            "[m] must be hidden when supports_modify is false; got:\n{output}"
        );
        assert!(
            output.contains("[y]"),
            "other shortcuts stay; got:\n{output}"
        );
    }

    #[test]
    fn wire_risk_overrides_tool_name_exec_classification() {
        // `bash` is exec-class by name, but the daemon's risk class is
        // authoritative: `read_only` downgrades it to a plain args display.
        let mut it = item(1, "bash");
        it.risk = Some("read_only".into());
        let rows = compute_body_rows(&it, 60);
        assert!(
            rows.iter().all(|(k, _)| *k != RowKind::Command),
            "wire risk=read_only must suppress exec classification: {rows:?}"
        );

        // Conversely an unknown tool name carrying risk=exec gets command
        // extraction even though the local table does not know it.
        let mut it = item(1, "mystery_shell");
        it.risk = Some("exec".into());
        let rows = compute_body_rows(&it, 60);
        assert!(
            rows.iter()
                .any(|(k, t)| *k == RowKind::Command && t.contains("$ ls -la")),
            "wire risk=exec must enable command extraction: {rows:?}"
        );
    }

    #[test]
    fn body_rows_are_memoised_per_item_and_width() {
        let it = item(1, "bash");
        let first = body_rows(&it, 40);
        assert!(
            it.rows_cache.borrow().is_some(),
            "first call populates the cache"
        );
        let second = body_rows(&it, 40);
        assert_eq!(first, second, "cache hit returns identical rows");
        // A different width re-wraps rather than reusing the wrong wrap.
        let wider = body_rows(&it, 80);
        assert_eq!(
            it.rows_cache.borrow().as_ref().map(|(w, _)| *w),
            Some(80),
            "cache key tracks the wrap width"
        );
        assert!(wider.len() <= first.len(), "wider wrap needs fewer rows");
    }

    #[test]
    fn empty_items_renders_nothing() {
        let output = render_widget(&[], false, "");
        assert!(
            !output.contains("COWORK"),
            "empty items must not render COWORK header"
        );
    }

    #[test]
    fn queue_count_shown_when_multiple_items() {
        let items = vec![item(1, "bash"), item(2, "edit_file"), item(3, "read")];
        let output = render_widget(&items, false, "");
        assert!(
            output.contains("+2"),
            "must show +2 queued for 3 items; got:\n{output}"
        );
    }

    #[test]
    fn overlay_rect_is_centred_and_bounded() {
        let parent = Rect::new(0, 0, 100, 30);
        let items = vec![item(1, "bash")];
        let r = overlay_rect(parent, &items);
        assert!(r.width <= parent.width);
        assert!(r.height <= parent.height);
        // x is centred: left margin ≈ right margin
        let left = r.x;
        let right = parent.width - r.x - r.width;
        assert!(
            left.abs_diff(right) <= 1,
            "rect must be horizontally centred"
        );
    }

    #[test]
    fn overlay_rect_grows_for_wrapped_content() {
        let parent = Rect::new(0, 0, 100, 40);
        let mut it = item(1, "bash");
        it.args_display =
            r#"{"cmd":"cargo test --workspace -- --nocapture --test-threads=1"}"#.into();
        it.reasoning =
            "running the full workspace test suite to validate the refactor before merging".into();
        let items = vec![it];
        let tall = overlay_rect(parent, &items);
        let flat = overlay_rect(parent, &[item(1, "read")]);
        assert!(
            tall.height > flat.height,
            "overlay must grow for long content: tall={} flat={}",
            tall.height,
            flat.height
        );
    }

    // Regression: `u16::clamp(min, max)` panics when min > max, so a parent
    // smaller than the 40×7 minimum (a tiny tmux split) crashed the TUI.
    #[test]
    fn overlay_rect_does_not_panic_on_tiny_parents() {
        let items = vec![item(1, "bash")];
        for (w, h) in [(30, 4), (10, 2), (1, 1), (0, 0), (39, 6)] {
            let parent = Rect::new(0, 0, w, h);
            let r = overlay_rect(parent, &items);
            assert!(r.width <= parent.width, "width bounded for {w}x{h}");
            assert!(r.height <= parent.height, "height bounded for {w}x{h}");
            assert!(
                r.x + r.width <= parent.x + parent.width
                    && r.y + r.height <= parent.y + parent.height,
                "rect stays inside parent for {w}x{h}"
            );
        }
        // Empty items take the default-height path — also clamp-safe.
        let r = overlay_rect(Rect::new(0, 0, 10, 2), &[]);
        assert!(r.height <= 2);
    }

    #[test]
    fn widget_renders_on_tiny_area_without_panic() {
        let items = vec![item(1, "bash")];
        // 30×4 and 10×2 are smaller than the overlay's own minimum; rendering
        // must not panic.
        let output = render_widget_at(&items, false, "", 4);
        assert!(!output.is_empty());
        let _ = render_widget_at(&items, false, "", 2);
    }

    #[test]
    fn body_rows_caps_runaway_payloads_with_marker() {
        let mut it = item(1, "bash");
        it.args_display = "x".repeat(BODY_ROWS_CAP * 64);
        it.reasoning = "y".repeat(BODY_ROWS_CAP * 64);
        let rows = body_rows(&it, 40);
        assert!(
            rows.len() <= BODY_ROWS_CAP,
            "body rows must be capped: {}",
            rows.len()
        );
        assert_eq!(
            rows.last().map(|(_, t)| t.as_str()),
            Some("\u{2026}"),
            "capped body ends with a … marker"
        );
    }
}
