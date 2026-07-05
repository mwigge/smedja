//! `MainPanel` widget — scrollable message area with diff-aware line styling.

use crate::theme::palette;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

mod blocks;
mod highlight;

#[cfg(test)]
use blocks::table_cells;
use blocks::{
    block_markdown_spans, diff_line_spans, inline_markdown_spans, is_diff_marker, is_table_row,
    render_math, table_row_spans,
};
pub(crate) use highlight::highlight_code;
#[cfg(test)]
use highlight::{apply_syntect, ts_lang, TsLang};
#[cfg(test)]
use ratatui::style::Color;

/// Hard-wraps a styled [`Line`] into one or more visual rows, each at most
/// `width` display columns, splitting spans at column boundaries while
/// preserving each span's style. An empty line yields a single empty row.
///
/// Hard (column) wrapping — not word wrapping — keeps the visual-row count exact
/// and independent of `ratatui`'s internal (feature-gated) layout measurement, so
/// the panel's scroll/follow math stays in sync with what is drawn.
fn wrap_line_to(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut cur_w = 0usize;
    for span in &line.spans {
        let style = span.style;
        let mut chunk = String::new();
        for ch in span.content.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + cw > width && cur_w > 0 {
                if !chunk.is_empty() {
                    rows.last_mut()
                        .unwrap()
                        .push(Span::styled(std::mem::take(&mut chunk), style));
                }
                rows.push(Vec::new());
                cur_w = 0;
            }
            chunk.push(ch);
            cur_w += cw;
        }
        if !chunk.is_empty() {
            rows.last_mut().unwrap().push(Span::styled(chunk, style));
        }
    }
    rows.into_iter().map(Line::from).collect()
}

/// Visual style classification for a single rendered line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineStyle {
    /// Plain message text.
    Normal,
    /// Line starting with `+` (diff addition).
    Added,
    /// Line starting with `-` (diff removal).
    Removed,
    /// Line inside a fenced code block.
    Code,
}

/// A single line of text with its rendering style.
#[derive(Debug, Clone)]
pub struct StyledLine {
    /// The text content of the line (plain, for clipboard / search).
    pub text: String,
    /// The visual style to apply when rendering (used when `spans` is None).
    pub style: LineStyle,
    /// Pre-built per-character coloured spans (syntax highlighting).
    /// When `Some`, `render` uses these instead of the flat `style`.
    pub spans: Option<Line<'static>>,
}

impl StyledLine {
    pub(crate) fn plain(text: String, style: LineStyle) -> Self {
        Self {
            text,
            style,
            spans: None,
        }
    }
}

/// Scrollable panel displaying styled message lines.
#[derive(Debug)]
pub struct MainPanel {
    lines: Vec<StyledLine>,
    /// First visible line index.
    pub scroll: usize,
    /// Watermark set by `/clear`; lines before this index are not rendered.
    pub display_start: usize,
    /// When `true`, the view stays pinned to the newest content (the streaming
    /// default). Manual scroll-up clears it; scrolling back to the bottom re-arms
    /// it. `scroll` is only consulted as the top anchor when `follow` is `false`.
    follow: bool,
    /// Whether the next pushed line should be treated as code.
    in_code_block: bool,
    /// Language tag from the opening fence (e.g. "rust"), empty if none.
    code_lang: String,
    /// Inner (border-excluded) rect of the last render — for mouse hit-testing.
    last_inner: Rect,
    /// Logical line index for each visual row drawn in the last render window,
    /// so a mouse `y` maps back to the line it sits on (accounts for wrapping).
    row_logical: Vec<usize>,
    /// `(char_start, char_end)` offsets into the logical line's text for each
    /// visual row drawn, so a mouse `x` maps to a character column (wrap-aware).
    row_charbounds: Vec<(usize, usize)>,
}

/// Returns the `[a, b)` character range of `line`'s `text` that falls inside the
/// `(anchor, end)` selection, or `None` when nothing on this line is selected.
/// Endpoints are `(line, char_col)`; order-independent.
fn selected_subrange(
    selection: Option<((usize, usize), (usize, usize))>,
    line: usize,
    text: &str,
) -> Option<(usize, usize)> {
    let (anc, end) = selection?;
    let (lo, hi) = if anc <= end { (anc, end) } else { (end, anc) };
    if line < lo.0 || line > hi.0 {
        return None;
    }
    let len = text.chars().count();
    let a = if line == lo.0 { lo.1.min(len) } else { 0 };
    let b = if line == hi.0 { hi.1.min(len) } else { len };
    if a < b {
        Some((a, b))
    } else {
        None
    }
}

/// Hard-wraps `text` to `width` display columns (matching [`wrap_line_to`]) and
/// returns the `(char_start, char_end)` range of each visual row. Used to map a
/// mouse column to a character offset for partial-line selection.
fn text_row_bounds(text: &str, width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let mut rows: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut idx = 0usize;
    let mut cur_w = 0usize;
    for ch in text.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + cw > width && cur_w > 0 {
            rows.push((start, idx));
            start = idx;
            cur_w = 0;
        }
        cur_w += cw;
        idx += 1;
    }
    rows.push((start, idx));
    rows
}

impl MainPanel {
    /// Creates a new, empty [`MainPanel`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            scroll: 0,
            display_start: 0,
            follow: true,
            in_code_block: false,
            code_lang: String::new(),
            last_inner: Rect::new(0, 0, 0, 0),
            row_logical: Vec::new(),
            row_charbounds: Vec::new(),
        }
    }

    /// Advances the display watermark to the current line count, hiding all
    /// previously pushed lines without discarding them.  Used by `/clear`.
    pub fn clear_display(&mut self) {
        self.display_start = self.lines.len();
        self.scroll = self.lines.len();
        self.follow = true;
    }

    /// Pushes a line of text, classifying its style automatically.
    ///
    /// - Lines beginning with `+` → [`LineStyle::Added`]
    /// - Lines beginning with `-` → [`LineStyle::Removed`]
    /// - A triple-backtick boundary toggles code mode; subsequent lines are
    ///   [`LineStyle::Code`] and highlighted via syntect if a language tag was
    ///   present on the opening fence.
    ///
    /// Auto-scrolls to follow new content when the view is already at the bottom.
    pub fn push_line(&mut self, text: String) {
        // Tool-result meta lines ("↳ ok · …") render dim and on their own line,
        // never as code/diff — short-circuit before fence/prefix classification.
        if !self.in_code_block && text.starts_with('↳') {
            let spans = Line::from(Span::styled(
                text.clone(),
                Style::default().add_modifier(Modifier::DIM),
            ));
            self.lines.push(StyledLine {
                text,
                style: LineStyle::Normal,
                spans: Some(spans),
            });
            if self.follow {
                self.scroll = self.lines.len().saturating_sub(1);
            }
            return;
        }

        // Detect fence boundaries (``` with optional language tag).
        if text.trim_start().starts_with("```") {
            if self.in_code_block {
                // Closing fence — push it as Code and exit code mode.
                self.lines.push(StyledLine::plain(text, LineStyle::Code));
                self.in_code_block = false;
                self.code_lang = String::new();
            } else {
                // Opening fence — record language (if any) and enter code mode.
                let lang = text.trim_start().trim_start_matches('`').trim().to_owned();
                self.code_lang = lang;
                self.in_code_block = true;
                self.lines.push(StyledLine::plain(text, LineStyle::Code));
            }
        } else if self.in_code_block {
            // Inside a fenced block: apply syntect if we know the language.
            if self.code_lang.eq_ignore_ascii_case("diff")
                || self.code_lang.eq_ignore_ascii_case("patch")
            {
                // Diff blocks get the gutter/header "card" treatment.
                let spans = diff_line_spans(&text);
                self.lines.push(StyledLine {
                    text,
                    style: LineStyle::Code,
                    spans: Some(spans),
                });
            } else if self.code_lang.is_empty() {
                self.lines.push(StyledLine::plain(text, LineStyle::Code));
            } else {
                let lang = self.code_lang.clone();
                let highlighted = highlight_code(&lang, &text);
                if highlighted.is_empty() {
                    self.lines.push(StyledLine::plain(text, LineStyle::Code));
                } else {
                    self.lines.extend(highlighted);
                }
            }
        } else if is_diff_marker(&text) {
            // Standalone diff header/hunk outside a fence — specific enough syntax
            // to style without false-positiving on prose.
            let spans = diff_line_spans(&text);
            self.lines.push(StyledLine {
                text,
                style: LineStyle::Normal,
                spans: Some(spans),
            });
        } else if is_table_row(&text) {
            // Markdown table row → cell separators / header rule. Keep the raw
            // markdown as the backing text so copy yields clean source.
            let spans = table_row_spans(&text);
            self.lines.push(StyledLine {
                text,
                style: LineStyle::Normal,
                spans: Some(spans),
            });
        } else if let Some(line) = block_markdown_spans(&text) {
            // Block-level markdown: heading, blockquote, thematic break, or list
            // item. Keep the displayed (marker-normalised) text as the backing
            // string so copy yields the rendered form.
            let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            self.lines.push(StyledLine {
                text: flat,
                style: LineStyle::Normal,
                spans: Some(line),
            });
        } else {
            // Outside code blocks: classify by prefix and apply math rendering.
            let style = if text.starts_with('+') {
                LineStyle::Added
            } else if text.starts_with('-') {
                LineStyle::Removed
            } else {
                LineStyle::Normal
            };
            let text = if text.contains('$') {
                render_math(&text)
            } else {
                text
            };
            // Inline markdown (bold/italic/`code`) only on plain prose lines —
            // diff +/- lines keep their flat add/remove colour.
            if matches!(style, LineStyle::Normal) {
                if let Some(line) = inline_markdown_spans(&text) {
                    let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                    self.lines.push(StyledLine {
                        text: flat,
                        style: LineStyle::Normal,
                        spans: Some(line),
                    });
                } else {
                    self.lines.push(StyledLine::plain(text, style));
                }
            } else {
                self.lines.push(StyledLine::plain(text, style));
            }
        }

        if self.follow {
            self.scroll = self.lines.len().saturating_sub(1);
        }
    }

    /// Pushes a pre-styled line (e.g. a tool-call card) as its own message line,
    /// bypassing automatic classification. The flattened text backs selection and
    /// search; `spans` carries the rendering.
    pub fn push_styled_line(&mut self, line: Line<'static>) {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        self.lines.push(StyledLine {
            text,
            style: LineStyle::Normal,
            spans: Some(line),
        });
        if self.follow {
            self.scroll = self.lines.len().saturating_sub(1);
        }
    }

    /// Replaces the styled content of line `idx` in place (e.g. updating a tool
    /// card from "running" to "done"). No-op if `idx` is out of range.
    pub fn replace_styled_line(&mut self, idx: usize, line: Line<'static>) {
        if let Some(slot) = self.lines.get_mut(idx) {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            *slot = StyledLine {
                text,
                style: LineStyle::Normal,
                spans: Some(line),
            };
        }
    }

    /// Re-runs full classification (fence/syntect/diff/math) on the last line.
    ///
    /// Streaming appends raw text to the tail line via [`push_delta`], which does
    /// not classify. Call this when a line completes (a newline arrives) so
    /// streamed code blocks get syntax-highlighted and diff lines get coloured —
    /// the same treatment [`push_line`] gives non-streamed content.
    pub fn finalize_last_line(&mut self) {
        if let Some(last) = self.lines.pop() {
            // A line that already carries explicit spans (a card, a result meta
            // line, or already-highlighted code) is left as-is.
            if last.spans.is_some() {
                self.lines.push(last);
                return;
            }
            self.push_line(last.text);
        }
    }

    /// Renders the panel into `frame` at `area`, respecting the scroll offset.
    ///
    /// `selection` highlights lines from `lo` to `hi` (inclusive) in reverse video.
    /// `search_query` highlights lines containing the query text (yellow background).
    /// `no_color` strips all colours when the `NO_COLOR` env var is set.
    pub fn render(
        &mut self,
        area: Rect,
        frame: &mut Frame,
        selection: Option<((usize, usize), (usize, usize))>,
        search_query: Option<&str>,
        no_color: bool,
    ) {
        let inner_w = area.width.saturating_sub(2) as usize; // subtract border cols
        let inner_h = area.height.saturating_sub(2) as usize; // subtract border rows
        let p = palette();

        let search_needle = search_query
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase);

        // Style one logical line into a single ratatui `Line` (selection / search
        // / cached rich spans / prefix classification), matching the previous
        // per-line rendering — applied before wrapping so styling survives it.
        let style_line = |abs_line: usize, sl: &StyledLine| -> Line<'static> {
            // Character-precise selection: split this line's text into
            // pre / selected / post spans (the selected span reverse-styled).
            if let Some((a, b)) = selected_subrange(selection, abs_line, &sl.text) {
                let chars: Vec<char> = sl.text.chars().collect();
                let hl = Style::default().fg(p.accent).bg(p.header);
                let mut spans: Vec<Span<'static>> = Vec::new();
                let pre: String = chars[..a].iter().collect();
                if !pre.is_empty() {
                    spans.push(Span::raw(pre));
                }
                spans.push(Span::styled(chars[a..b].iter().collect::<String>(), hl));
                let post: String = chars[b..].iter().collect();
                if !post.is_empty() {
                    spans.push(Span::raw(post));
                }
                return Line::from(spans);
            }
            let is_search_match = search_needle
                .as_deref()
                .is_some_and(|q| sl.text.to_lowercase().contains(q));

            if is_search_match {
                Line::from(Span::styled(
                    sl.text.clone(),
                    Style::default().fg(p.bg).bg(p.text_bright),
                ))
            } else if let Some(ref rich) = sl.spans {
                if no_color {
                    let text = rich
                        .spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>();
                    Line::raw(text)
                } else {
                    rich.clone()
                }
            } else {
                let base = if no_color {
                    Style::default()
                } else {
                    match sl.style {
                        LineStyle::Normal => Style::default(),
                        LineStyle::Added => Style::default().fg(p.code_added),
                        LineStyle::Removed => Style::default().fg(p.code_removed),
                        LineStyle::Code => Style::default().fg(p.code_default),
                    }
                };
                Line::from(Span::styled(sl.text.clone(), base))
            }
        };

        // Wrap every visible logical line into visual rows, tracking the visual
        // offset at which the `scroll` anchor line begins (used when not
        // following the bottom) and the logical line behind each visual row (for
        // mouse hit-testing).
        let mut visual: Vec<Line<'static>> = Vec::new();
        let mut visual_logical: Vec<usize> = Vec::new();
        let mut visual_bounds: Vec<(usize, usize)> = Vec::new();
        let mut scroll_visual_start = 0usize;
        for (abs_line, sl) in self.lines.iter().enumerate().skip(self.display_start) {
            if abs_line == self.scroll {
                scroll_visual_start = visual.len();
            }
            let styled = style_line(abs_line, sl);
            let bounds = text_row_bounds(&sl.text, inner_w);
            let end_char = sl.text.chars().count();
            for (i, vrow) in wrap_line_to(&styled, inner_w).into_iter().enumerate() {
                visual.push(vrow);
                visual_logical.push(abs_line);
                // Styled rows can exceed text rows (e.g. diff gutter) — clamp.
                visual_bounds.push(bounds.get(i).copied().unwrap_or((end_char, end_char)));
            }
        }

        let total = visual.len();
        let max_off = total.saturating_sub(inner_h);
        let start = if self.follow {
            max_off
        } else {
            scroll_visual_start.min(max_off)
        };
        let end = (start + inner_h).min(total);
        let window: Vec<Line<'static>> = visual.get(start..end).unwrap_or(&[]).to_vec();

        // Cache the inner rect and the per-visual-row → logical-line map so a
        // later mouse click can resolve which message line it landed on.
        #[allow(clippy::cast_possible_truncation)]
        let (panel_w, panel_h) = (inner_w as u16, inner_h as u16);
        self.last_inner = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            panel_w,
            panel_h,
        );
        self.row_logical = visual_logical.get(start..end).unwrap_or(&[]).to_vec();
        self.row_charbounds = visual_bounds.get(start..end).unwrap_or(&[]).to_vec();

        let widget = Paragraph::new(window).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(p.border))
                .title(" messages "),
        );
        frame.render_widget(widget, area);
    }

    /// Maps a terminal `(x, y)` cell — as delivered by a mouse event — to a
    /// `(logical_line, char_column)` position in the last render, or `None` when
    /// the point is outside the inner area / past the last drawn row. Wrap-aware:
    /// `x` resolves to the character offset within the clicked visual row, and a
    /// click past the end of a row's text clamps to that row's last character.
    #[must_use]
    pub fn pos_at(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let r = self.last_inner;
        if r.width == 0 || r.height == 0 || x < r.x || y < r.y || y >= r.y + r.height {
            return None;
        }
        let row = (y - r.y) as usize;
        let line = *self.row_logical.get(row)?;
        let (cs, ce) = *self.row_charbounds.get(row)?;
        let text = &self.lines.get(line)?.text;
        let col_disp = usize::from(x.saturating_sub(r.x));
        let mut acc = 0usize;
        let mut col = cs;
        for ch in text.chars().skip(cs).take(ce.saturating_sub(cs)) {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if acc + cw > col_disp {
                break;
            }
            acc += cw;
            col += 1;
        }
        Some((line, col.min(ce)))
    }

    /// Like [`Self::pos_at`], but clamps the pointer into the panel so a drag
    /// that runs past the top or bottom edge still resolves to the first/last
    /// visible position. Returns `None` only before the panel has an area.
    /// Used together with [`Self::row_above`]/[`Self::row_below`] for
    /// auto-scrolling drag-selection.
    #[must_use]
    pub fn pos_at_clamped(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let r = self.last_inner;
        if r.width == 0 || r.height == 0 {
            return None;
        }
        let cy = y.clamp(r.y, r.y + r.height - 1);
        self.pos_at(x.max(r.x), cy)
    }

    /// True when row `y` sits above the panel's visible area (drag past the top).
    #[must_use]
    pub fn row_above(&self, y: u16) -> bool {
        let r = self.last_inner;
        r.height != 0 && y < r.y
    }

    /// True when row `y` sits below the panel's visible area (drag past the bottom).
    #[must_use]
    pub fn row_below(&self, y: u16) -> bool {
        let r = self.last_inner;
        r.height != 0 && y >= r.y + r.height
    }

    /// Number of characters in logical line `idx` (0 if out of range) — lets the
    /// keyboard visual mode select whole lines by column.
    #[must_use]
    pub fn line_char_len(&self, idx: usize) -> usize {
        self.lines.get(idx).map_or(0, |l| l.text.chars().count())
    }

    /// Extracts the text covered by a `(anchor, end)` character selection, joining
    /// across lines with newlines. Order-independent.
    #[must_use]
    pub fn selection_text(&self, anchor: (usize, usize), end: (usize, usize)) -> String {
        let (lo, hi) = if anchor <= end {
            (anchor, end)
        } else {
            (end, anchor)
        };
        let mut out = String::new();
        for line in lo.0..=hi.0 {
            let Some(sl) = self.lines.get(line) else {
                continue;
            };
            let chars: Vec<char> = sl.text.chars().collect();
            let len = chars.len();
            let a = if line == lo.0 { lo.1.min(len) } else { 0 };
            let b = if line == hi.0 { hi.1.min(len) } else { len };
            if a < b {
                out.extend(&chars[a..b]);
            }
            if line != hi.0 {
                out.push('\n');
            }
        }
        out
    }

    /// Returns the number of stored lines.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Returns `true` when no lines have been pushed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn scroll_up(&mut self) {
        // Any manual upward scroll detaches from the bottom-follow.
        self.follow = false;
        self.scroll = self.scroll.saturating_sub(1).max(self.display_start);
    }

    pub fn scroll_down(&mut self) {
        let max = self.lines.len().saturating_sub(1);
        if self.scroll < max {
            self.scroll += 1;
        }
        // Reaching the bottom re-arms follow so new content tracks again.
        if self.scroll >= max {
            self.follow = true;
        }
    }

    pub fn scroll_to_top(&mut self) {
        self.follow = false;
        self.scroll = self.display_start;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.follow = true;
        self.scroll = self.lines.len().saturating_sub(1);
    }

    /// Clamps `scroll` to the valid range after a resize.
    pub fn clamp_scroll(&mut self) {
        let max = self.lines.len().saturating_sub(1).max(self.display_start);
        if self.scroll > max {
            self.scroll = max;
        }
    }

    /// Returns the text of lines from `from` to `to` (inclusive, either order).
    #[must_use]
    pub fn lines_text(&self, from: usize, to: usize) -> Vec<String> {
        let lo = from.min(to);
        let hi = (from.max(to) + 1).min(self.lines.len());
        self.lines[lo..hi].iter().map(|l| l.text.clone()).collect()
    }

    /// Appends `text` to the last line if one exists, or creates a new one.
    ///
    /// Used for streaming turn responses where content arrives incrementally.
    /// Each call extends the tail of the current in-progress line rather than
    /// opening a new one, so callers can accumulate partial content until the
    /// turn completes and `push_line` takes over for the final render.
    pub fn push_delta(&mut self, text: &str) {
        if let Some(last) = self.lines.last_mut() {
            last.text.push_str(text);
            // Clear cached spans since text changed.
            last.spans = None;
        } else {
            self.lines
                .push(StyledLine::plain(text.to_owned(), LineStyle::Normal));
        }
    }

    /// Returns the full text content of all stored lines joined by newlines.
    ///
    /// Only compiled under `#[cfg(test)]` — used by unit tests to inspect panel
    /// state without coupling them to the internal `Vec<StyledLine>` layout.
    #[cfg(test)]
    #[must_use]
    pub fn visible_text(&self) -> String {
        self.lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Default for MainPanel {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
