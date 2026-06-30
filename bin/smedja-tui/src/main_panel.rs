//! `MainPanel` widget — scrollable message area with diff-aware line styling.

use crate::theme::palette;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use similar::{ChangeTag, TextDiff};
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use unicode_width::UnicodeWidthChar;

static SYNTAX_SET: OnceLock<syntect::parsing::SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<syntect::highlighting::ThemeSet> = OnceLock::new();

/// Byte length at which a pending word is force-flushed even without a space
/// boundary — prevents unbounded buffering of CJK text and long URLs.
const WORD_FLUSH_BYTES: usize = 40;

// ---------------------------------------------------------------------------
// M15 — Word-level diff highlighting using the `similar` crate
// ---------------------------------------------------------------------------

/// Produces word-level diff spans by comparing `before` and `after` word by
/// word.  Words present only in `after` are styled with a bright green
/// foreground (added); words only in `before` are styled with a bright red
/// strikethrough (removed).  Unchanged words keep the default style.
///
/// Designed for inline diff display on single `+`/`-` lines where both
/// versions of the line are known.
#[must_use]
pub fn word_diff_spans(before: &str, after: &str) -> Line<'static> {
    let p = palette();
    let diff = TextDiff::from_words(before, after);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for change in diff.iter_all_changes() {
        let word = change.value().to_owned();
        let span = match change.tag() {
            ChangeTag::Equal => Span::raw(word),
            ChangeTag::Insert => Span::styled(word, Style::default().fg(p.code_added)),
            ChangeTag::Delete => Span::styled(
                word,
                Style::default()
                    .fg(p.code_removed)
                    .add_modifier(Modifier::CROSSED_OUT),
            ),
        };
        spans.push(span);
    }
    Line::from(spans)
}

// ---------------------------------------------------------------------------
// M21 — Image detection and Kitty/placeholder rendering
// ---------------------------------------------------------------------------

/// Detects whether `line` contains an image reference — either a `data:image/`
/// URI or a file path ending in a recognised image extension.
///
/// Returns the image source string (the URI or path) if found, or `None` for
/// plain text.
#[must_use]
pub fn detect_image_in_line(line: &str) -> Option<&str> {
    // File path extensions that indicate an image.
    const IMAGE_EXTS: &[&str] = &[".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg"];

    // data: URI check — e.g. `data:image/png;base64,...`
    if let Some(start) = line.find("data:image/") {
        // Scan for the end of the URI (whitespace or `"` or `'` or `)`).
        let src = &line[start..];
        let end = src
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')'))
            .unwrap_or(src.len());
        return Some(&src[..end]);
    }
    // File path with image extension — match space-delimited token.
    for token in line.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        let lower = lower.trim_matches(|c: char| matches!(c, '"' | '\'' | '(' | ')' | '[' | ']'));
        if IMAGE_EXTS.iter().any(|ext| lower.ends_with(ext)) {
            // Return the original (un-lowercased) token slice.
            let start = line.find(token).unwrap_or(0);
            return Some(&line[start..start + token.len()]);
        }
    }
    None
}

/// Returns a text placeholder for an image that cannot be rendered as pixels
/// (e.g. when Kitty graphics protocol or Sixel is unavailable).
///
/// Format: `[image: <src>]`
#[must_use]
pub fn render_image_placeholder(src: &str) -> String {
    format!("[image: {src}]")
}

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

/// True for unified-diff header/hunk lines whose syntax is specific enough to
/// recognise outside a fenced block without false-positiving on prose. Content
/// (`+`/`-`) lines are intentionally excluded — bare `+`/`-` outside a diff is
/// handled by the existing add/remove classification.
fn is_diff_marker(text: &str) -> bool {
    if text.starts_with("@@ ") && text[3..].contains("@@") {
        return true;
    }
    if text.starts_with("diff --git ") {
        return true;
    }
    // `--- path` / `+++ path`: require a path-like first token so prose such as
    // "--- a thought ---" is not mistaken for a diff file header.
    for pre in ["--- ", "+++ "] {
        if let Some(rest) = text.strip_prefix(pre) {
            let tok = rest.split_whitespace().next().unwrap_or("");
            if tok.contains('/') || tok == "/dev/null" {
                return true;
            }
        }
    }
    false
}

/// Parses inline markdown in a prose line — `` `code` ``, `**bold**`, `*italic*`
/// — into styled spans, returning `None` when there is no markup (so the common
/// path stays a cheap plain line). Conservative to avoid false positives: emphasis
/// markers must hug non-space text (`CommonMark` flanking), so `a * b` and bullet
/// `* item` are left alone. The returned line's flattened text drops the markers,
/// matching what is displayed (and what gets copied).
#[allow(clippy::many_single_char_names)]
fn inline_markdown_spans(text: &str) -> Option<Line<'static>> {
    let p = palette();
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut i = 0usize;
    let mut found = false;

    let nonspace = |c: char| !c.is_whitespace();

    while i < n {
        let c = chars[i];
        // `inline code`
        if c == '`' {
            if let Some(close) = (i + 1..n).find(|&j| chars[j] == '`') {
                if close > i + 1 {
                    if !buf.is_empty() {
                        spans.push(Span::raw(std::mem::take(&mut buf)));
                    }
                    let code: String = chars[i + 1..close].iter().collect();
                    spans.push(Span::styled(code, Style::default().fg(p.code_default)));
                    i = close + 1;
                    found = true;
                    continue;
                }
            }
        }
        // **bold**
        if c == '*' && i + 1 < n && chars[i + 1] == '*' && i + 2 < n && nonspace(chars[i + 2]) {
            let mut j = i + 2;
            let mut close = None;
            while j + 1 < n {
                if chars[j] == '*' && chars[j + 1] == '*' && nonspace(chars[j - 1]) {
                    close = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(close) = close {
                if !buf.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut buf)));
                }
                let inner: String = chars[i + 2..close].iter().collect();
                spans.push(Span::styled(
                    inner,
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                i = close + 2;
                found = true;
                continue;
            }
        }
        // *italic*
        if c == '*' && i + 1 < n && nonspace(chars[i + 1]) {
            if let Some(close) = (i + 1..n).find(|&j| chars[j] == '*' && nonspace(chars[j - 1])) {
                if close > i + 1 {
                    if !buf.is_empty() {
                        spans.push(Span::raw(std::mem::take(&mut buf)));
                    }
                    let inner: String = chars[i + 1..close].iter().collect();
                    spans.push(Span::styled(
                        inner,
                        Style::default().add_modifier(Modifier::ITALIC),
                    ));
                    i = close + 1;
                    found = true;
                    continue;
                }
            }
        }
        buf.push(c);
        i += 1;
    }
    if !buf.is_empty() {
        spans.push(Span::raw(buf));
    }
    if found {
        Some(Line::from(spans))
    } else {
        None
    }
}

/// True for a markdown table row — a trimmed line that starts with `|` and has
/// at least one more `|`. Requiring a leading pipe avoids false-positiving on
/// prose that merely contains a pipe.
fn is_table_row(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('|') && t.get(1..).is_some_and(|rest| rest.contains('|'))
}

/// Splits a table row into trimmed cell texts (outer pipes stripped).
fn table_cells(text: &str) -> Vec<String> {
    let t = text.trim();
    let inner = t.strip_prefix('|').unwrap_or(t);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner.split('|').map(|c| c.trim().to_owned()).collect()
}

/// Renders a markdown table row: a `|---|` delimiter row becomes a horizontal
/// rule, any other row becomes its cells joined by a dim `│`. (Columns are not
/// auto-aligned across rows — a per-line pass that streams cleanly.)
fn table_row_spans(text: &str) -> Line<'static> {
    let p = palette();
    let sep = Style::default().fg(p.border);
    let cells = table_cells(text);
    let is_delim = !cells.is_empty()
        && cells.iter().all(|c| {
            c.contains('-')
                && c.chars()
                    .all(|ch| ch == '-' || ch == ':' || ch.is_whitespace())
        });
    let mut spans: Vec<Span<'static>> = Vec::new();
    if is_delim {
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled("┼", sep));
            }
            spans.push(Span::styled("─".repeat(c.chars().count().max(3)), sep));
        }
    } else {
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" │ ", sep));
            }
            spans.push(Span::raw(c.clone()));
        }
    }
    Line::from(spans)
}

/// Renders one line of a unified diff the "card" way: a coloured left gutter bar
/// for added/removed/context lines, bold dim file/meta headers, and an accented
/// hunk (`@@ … @@`) header. Display-only — the panel keeps the original text for
/// selection/copy, so the gutter never pollutes yanked content.
fn diff_line_spans(text: &str) -> Line<'static> {
    let p = palette();
    let header = Style::default().fg(p.text_dim).add_modifier(Modifier::BOLD);
    let hunk = Style::default().fg(p.accent).add_modifier(Modifier::BOLD);

    if text.starts_with("@@") {
        return Line::from(Span::styled(text.to_owned(), hunk));
    }
    if text.starts_with("diff --git")
        || text.starts_with("index ")
        || text.starts_with("--- ")
        || text.starts_with("+++ ")
        || text.starts_with("rename ")
        || text.starts_with("similarity ")
        || text.starts_with("new file")
        || text.starts_with("deleted file")
    {
        return Line::from(Span::styled(text.to_owned(), header));
    }
    let body = if text.starts_with('+') {
        Style::default().fg(p.code_added)
    } else if text.starts_with('-') {
        Style::default().fg(p.code_removed)
    } else {
        Style::default().fg(p.text_dim)
    };
    let bar = if text.starts_with('+') || text.starts_with('-') {
        "▎"
    } else {
        " "
    };
    Line::from(vec![
        Span::styled(bar, body),
        Span::styled(text.to_owned(), body),
    ])
}

/// Detects the programming language from the first few lines of code content.
///
/// Checks shebangs first, then keyword patterns.  Returns a `'static` string
/// suitable for passing to `apply_syntect`, or `""` when detection fails.
#[must_use]
pub fn detect_lang(lines: &[&str]) -> &'static str {
    // 1. Shebang on the first non-empty line.
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(interp) = trimmed.strip_prefix("#!") {
            if interp.contains("python") {
                return "python";
            }
            if interp.contains("/bash") || interp == "/bin/sh" || interp.ends_with(" sh") {
                return "bash";
            }
            if interp.contains("node") {
                return "javascript";
            }
        }
        break; // only inspect the first non-empty line for shebangs
    }

    // Collect all lines into a single text for keyword checks.
    let combined = lines.join("\n");

    // 2. Keyword heuristics (priority order matters — check Rust before others).
    if combined.contains("fn main()")
        || combined.contains("impl ")
        || combined.contains("let mut ")
        || combined.contains("pub fn ")
    {
        return "rust";
    }
    if combined.contains("func ") && combined.contains("package ") {
        return "go";
    }
    if combined.contains("def ") && combined.contains("import ") {
        return "python";
    }
    let upper = combined.to_uppercase();
    if upper.contains("SELECT ") || upper.contains("INSERT INTO") {
        return "sql";
    }
    if combined.contains("class ") && combined.contains("public static void") {
        return "java";
    }
    if combined.contains("const ") && combined.contains("function ") {
        return "javascript";
    }

    ""
}

// ---------------------------------------------------------------------------
// M16 — Cell spacing modes
// ---------------------------------------------------------------------------

/// Controls how much vertical breathing room is inserted between structural
/// elements (chips, tool-result cards, seam dividers) during rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Spacing {
    /// No blank rows inserted anywhere.
    Compact,
    /// One blank row after each chip (styled card).
    #[default]
    Comfortable,
    /// Two blank rows after each chip.
    Spacious,
}

impl Spacing {
    /// Returns the number of blank rows to insert after a chip line.
    #[must_use]
    pub fn blank_rows_after_chip(self) -> usize {
        match self {
            Self::Compact => 0,
            Self::Comfortable => 1,
            Self::Spacious => 2,
        }
    }

    /// Cycles to the next spacing mode: Compact → Comfortable → Spacious → Compact.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Compact => Self::Comfortable,
            Self::Comfortable => Self::Spacious,
            Self::Spacious => Self::Compact,
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
    /// Vertical spacing mode between structural elements.
    pub spacing: Spacing,
    /// Whether the next pushed line should be treated as code.
    in_code_block: bool,
    /// Language tag from the opening fence (e.g. "rust"), empty if none.
    code_lang: String,
    /// Buffered code lines for an unlabeled fence, used for retroactive
    /// language detection when the closing fence arrives.
    code_buf: Vec<String>,
    /// Index into `lines` where the buffered (unlabeled) code lines start,
    /// so they can be retroactively re-highlighted on fence close.
    code_buf_start: usize,
    /// Partial word held back during streaming so flushes happen at word
    /// boundaries.  When a space arrives the buffered prefix (up to and
    /// including the last space) is committed to the tail line; the suffix
    /// (the new partial word) stays here until the next space.
    pending_word: String,
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
            spacing: Spacing::Comfortable,
            in_code_block: false,
            code_lang: String::new(),
            code_buf: Vec::new(),
            code_buf_start: 0,
            pending_word: String::new(),
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

    /// Inserts a full-width context-seam divider — a dim horizontal rule that
    /// signals the model's active context window was replaced with a summary.
    /// Includes an optional token count for the summary.
    pub fn push_seam(&mut self, summary_tokens: usize) {
        let p = palette();
        let label = if summary_tokens > 0 {
            format!(" \u{21a9} context summarized ({summary_tokens} tok) ")
        } else {
            " \u{21a9} context summarized ".to_owned()
        };
        let rule = "\u{2500}".repeat(4);
        let text = format!("{rule}{label}{rule}");
        let spans = Line::from(Span::styled(
            text.clone(),
            Style::default()
                .fg(p.text_dim)
                .add_modifier(Modifier::ITALIC),
        ));
        self.lines.push(StyledLine {
            text,
            style: LineStyle::Normal,
            spans: Some(spans),
        });
        if self.follow {
            self.scroll = self.lines.len().saturating_sub(1);
        }
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
    #[allow(clippy::too_many_lines)] // fence handling + retroactive detection; splitting would obscure the flow
    pub fn push_line(&mut self, text: String) {
        // Image references are intercepted early — any prose line containing a
        // data URI or file path with a known image extension is replaced with a
        // text placeholder.  Guarded by `!in_code_block` so that image paths in
        // code or diff fences are left for syntax highlighting.
        if !self.in_code_block {
            if let Some(src) = detect_image_in_line(&text) {
                let placeholder = render_image_placeholder(src);
                self.lines
                    .push(StyledLine::plain(placeholder, LineStyle::Normal));
                if self.follow {
                    self.scroll = self.lines.len().saturating_sub(1);
                }
                return;
            }
        }

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
                // Closing fence — if we buffered unlabeled code lines, try to
                // detect the language and retroactively re-highlight them.
                if self.code_lang.is_empty() && !self.code_buf.is_empty() {
                    let buf_refs: Vec<&str> = self.code_buf.iter().map(String::as_str).collect();
                    let detected = detect_lang(&buf_refs);
                    if !detected.is_empty() {
                        // Re-highlight all buffered lines in place.
                        let start = self.code_buf_start;
                        let end = start + self.code_buf.len();
                        let highlighted_all: Vec<StyledLine> = self
                            .code_buf
                            .iter()
                            .flat_map(|l| {
                                let h = apply_syntect(detected, l);
                                if h.is_empty() {
                                    vec![StyledLine::plain(l.clone(), LineStyle::Code)]
                                } else {
                                    h
                                }
                            })
                            .collect();
                        // Replace the plain Code lines with highlighted ones.
                        self.lines.splice(start..end, highlighted_all);
                    }
                }
                self.code_buf.clear();
                self.code_buf_start = 0;
                // Push closing fence and exit code mode.
                self.lines.push(StyledLine::plain(text, LineStyle::Code));
                self.in_code_block = false;
                self.code_lang = String::new();
            } else {
                // Opening fence — record language (if any) and enter code mode.
                let lang = text.trim_start().trim_start_matches('`').trim().to_owned();
                self.code_lang = lang;
                self.in_code_block = true;
                self.lines.push(StyledLine::plain(text, LineStyle::Code));
                // If unlabeled, note where buffered lines will start.
                if self.code_lang.is_empty() {
                    self.code_buf_start = self.lines.len();
                }
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
                // Buffer this line for retroactive detection at fence close.
                self.code_buf.push(text.clone());
                self.lines.push(StyledLine::plain(text, LineStyle::Code));
            } else {
                let lang = self.code_lang.clone();
                let highlighted = apply_syntect(&lang, &text);
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
            } else if matches!(style, LineStyle::Added) {
                // Word-level diff highlight on added lines: compare empty baseline
                // to the added text so every word shows as "added" (green).
                let spans = Some(word_diff_spans("", &text));
                self.lines.push(StyledLine {
                    text,
                    style: LineStyle::Added,
                    spans,
                });
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
        // Drain any partial word that was still buffered (no trailing space yet).
        if !self.pending_word.is_empty() {
            let pending = std::mem::take(&mut self.pending_word);
            if let Some(last) = self.lines.last_mut() {
                last.text.push_str(&pending);
                last.spans = None;
            } else {
                self.lines
                    .push(StyledLine::plain(pending, LineStyle::Normal));
            }
        }
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
        // Append incoming text to the pending-word buffer first.
        self.pending_word.push_str(text);

        // Flush everything up to and including the last space in the buffer.
        // The suffix after the last space is the new partial word; keep it pending.
        if let Some(last_space) = self.pending_word.rfind(' ') {
            // Split at last_space + 1 so the space itself is flushed.
            let flush_end = last_space + 1;
            let flushed = self.pending_word[..flush_end].to_owned();
            let remainder = self.pending_word[flush_end..].to_owned();
            self.pending_word = remainder;

            if let Some(last) = self.lines.last_mut() {
                last.text.push_str(&flushed);
                // Clear cached spans since text changed.
                last.spans = None;
            } else {
                self.lines
                    .push(StyledLine::plain(flushed, LineStyle::Normal));
            }
        }

        // Secondary flush: CJK text and long URLs never contain spaces, so the
        // pending buffer would grow without bound. Flush once the buffer exceeds
        // the threshold so the display stays responsive.
        if self.pending_word.len() > WORD_FLUSH_BYTES {
            // Flush long token (CJK, URLs) to avoid unbounded buffering.
            let flushed = std::mem::take(&mut self.pending_word);
            if let Some(last) = self.lines.last_mut() {
                last.text.push_str(&flushed);
                last.spans = None;
            } else {
                self.lines
                    .push(StyledLine::plain(flushed, LineStyle::Normal));
            }
        }
    }

    /// Returns the full text content of all stored lines joined by newlines.
    ///
    /// Only compiled under `#[cfg(test)]` — used by unit tests to inspect panel
    /// state without coupling them to the internal `Vec<StyledLine>` layout.
    #[cfg(test)]
    #[must_use]
    pub fn visible_text(&self) -> String {
        let mut parts: Vec<&str> = self.lines.iter().map(|l| l.text.as_str()).collect();
        // Include any partial word that is still pending (not yet space-flushed).
        let pending = self.pending_word.as_str();
        if !pending.is_empty() {
            if parts.is_empty() {
                // No committed lines yet — return just the pending word.
                return pending.to_owned();
            }
            // Append pending word to the last line for display purposes.
            let last = parts.pop().unwrap_or("");
            let combined = format!("{last}{pending}");
            let mut result = parts.join("\n");
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&combined);
            return result;
        }
        parts.join("\n")
    }
}

impl Default for MainPanel {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Math rendering (LaTeX inline → Unicode)
// ---------------------------------------------------------------------------

/// Converts `$...$` inline LaTeX math spans in `text` to Unicode equivalents.
///
/// Only common Greek letters, operators, arrows, and digit super/subscripts are
/// converted.  Unrecognised sequences are left verbatim.  Dollar signs
/// surrounding the math are stripped.  `$$...$$` display-math is also handled
/// (treated the same as inline).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn render_math(text: &str) -> String {
    // Symbol table: LaTeX command (without backslash) → Unicode string.
    const SYMBOLS: &[(&str, &str)] = &[
        // Greek lowercase
        ("alpha", "α"),
        ("beta", "β"),
        ("gamma", "γ"),
        ("delta", "δ"),
        ("epsilon", "ε"),
        ("zeta", "ζ"),
        ("eta", "η"),
        ("theta", "θ"),
        ("lambda", "λ"),
        ("mu", "μ"),
        ("nu", "ν"),
        ("pi", "π"),
        ("rho", "ρ"),
        ("sigma", "σ"),
        ("tau", "τ"),
        ("phi", "φ"),
        ("chi", "χ"),
        ("psi", "ψ"),
        ("omega", "ω"),
        // Greek uppercase
        ("Gamma", "Γ"),
        ("Delta", "Δ"),
        ("Theta", "Θ"),
        ("Lambda", "Λ"),
        ("Pi", "Π"),
        ("Sigma", "Σ"),
        ("Phi", "Φ"),
        ("Omega", "Ω"),
        // Operators
        ("sum", "Σ"),
        ("prod", "Π"),
        ("int", "∫"),
        ("sqrt", "√"),
        ("infty", "∞"),
        ("partial", "∂"),
        ("nabla", "∇"),
        ("pm", "±"),
        ("times", "×"),
        ("cdot", "·"),
        ("leq", "≤"),
        ("geq", "≥"),
        ("neq", "≠"),
        ("approx", "≈"),
        ("equiv", "≡"),
        ("in", "∈"),
        ("notin", "∉"),
        ("subset", "⊂"),
        ("cup", "∪"),
        ("cap", "∩"),
        // Arrows
        ("to", "→"),
        ("leftarrow", "←"),
        ("Rightarrow", "⇒"),
        ("Leftarrow", "⇐"),
        ("iff", "⟺"),
        // Misc
        ("cdots", "⋯"),
        ("ldots", "…"),
        ("forall", "∀"),
        ("exists", "∃"),
    ];
    // Superscript digit map.
    const SUPERSCRIPTS: &[(char, char)] = &[
        ('0', '⁰'),
        ('1', '¹'),
        ('2', '²'),
        ('3', '³'),
        ('4', '⁴'),
        ('5', '⁵'),
        ('6', '⁶'),
        ('7', '⁷'),
        ('8', '⁸'),
        ('9', '⁹'),
    ];
    // Subscript digit map.
    const SUBSCRIPTS: &[(char, char)] = &[
        ('0', '₀'),
        ('1', '₁'),
        ('2', '₂'),
        ('3', '₃'),
        ('4', '₄'),
        ('5', '₅'),
        ('6', '₆'),
        ('7', '₇'),
        ('8', '₈'),
        ('9', '₉'),
    ];

    fn expand_math(math: &str) -> String {
        let mut out = String::with_capacity(math.len());
        let mut chars = math.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    let mut cmd = String::new();
                    while let Some(&ch) = chars.peek() {
                        if ch.is_alphabetic() {
                            cmd.push(ch);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if let Some(&(_, sym)) = SYMBOLS.iter().find(|(k, _)| *k == cmd) {
                        out.push_str(sym);
                    } else {
                        out.push('\\');
                        out.push_str(&cmd);
                    }
                }
                '^' => {
                    if let Some(&digit) = chars.peek().filter(|ch| ch.is_ascii_digit()) {
                        chars.next();
                        if let Some(&(_, sup)) = SUPERSCRIPTS.iter().find(|(d, _)| *d == digit) {
                            out.push(sup);
                        } else {
                            out.push('^');
                            out.push(digit);
                        }
                    } else {
                        out.push('^');
                    }
                }
                '_' => {
                    if let Some(&digit) = chars.peek().filter(|ch| ch.is_ascii_digit()) {
                        chars.next();
                        if let Some(&(_, sub)) = SUBSCRIPTS.iter().find(|(d, _)| *d == digit) {
                            out.push(sub);
                        } else {
                            out.push('_');
                            out.push(digit);
                        }
                    } else {
                        out.push('_');
                    }
                }
                other => out.push(other),
            }
        }
        out
    }

    // Scan for $...$ spans; handle $$ as well (same treatment).
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(start) = remaining.find('$') {
        result.push_str(&remaining[..start]);
        remaining = &remaining[start..];
        // Check for $$
        let (skip, close) = if remaining.starts_with("$$") {
            (2usize, "$$")
        } else {
            (1, "$")
        };
        remaining = &remaining[skip..];
        if let Some(end) = remaining.find(close) {
            let math = &remaining[..end];
            result.push_str(&expand_math(math));
            remaining = &remaining[end + close.len()..];
        } else {
            // No closing delimiter — push the dollar sign(s) and continue.
            for _ in 0..skip {
                result.push('$');
            }
        }
    }
    result.push_str(remaining);
    result
}

// ---------------------------------------------------------------------------
// Syntect integration
// ---------------------------------------------------------------------------

/// Highlights `code` for the given `lang` using syntect, returning one
/// [`StyledLine`] per input line with per-character [`Color::Rgb`] spans.
///
/// Falls back to plain [`LineStyle::Code`] lines when the language is unknown
/// or highlighting fails.
#[must_use]
pub fn apply_syntect(lang: &str, code: &str) -> Vec<StyledLine> {
    let ss = SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines);
    let ts = THEME_SET.get_or_init(ThemeSet::load_defaults);

    let syntax = ss
        .find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_extension(lang))
        .unwrap_or_else(|| ss.find_syntax_plain_text());

    let theme = ts
        .themes
        .get("base16-ocean.dark")
        .or_else(|| ts.themes.values().next())
        .cloned();

    let Some(theme) = theme else {
        return code
            .lines()
            .map(|l| StyledLine::plain(l.to_owned(), LineStyle::Code))
            .collect();
    };

    let mut highlighter = HighlightLines::new(syntax, &theme);
    let mut result = Vec::new();

    for line in syntect::util::LinesWithEndings::from(code) {
        let text = line.trim_end_matches(['\n', '\r']).to_owned();
        match highlighter.highlight_line(line, ss) {
            Ok(regions) => {
                let spans: Vec<Span<'static>> = regions
                    .iter()
                    .filter_map(|(style, fragment)| {
                        let s = fragment.trim_end_matches(['\n', '\r']);
                        if s.is_empty() {
                            return None;
                        }
                        let fg = style.foreground;
                        let color = Color::Rgb(fg.r, fg.g, fg.b);
                        Some(Span::styled(s.to_owned(), Style::default().fg(color)))
                    })
                    .collect();
                result.push(StyledLine {
                    text,
                    style: LineStyle::Code,
                    spans: if spans.is_empty() {
                        None
                    } else {
                        Some(Line::from(spans))
                    },
                });
            }
            Err(_) => {
                result.push(StyledLine::plain(text, LineStyle::Code));
            }
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // L121: +foo → LineStyle::Added
    #[test]
    fn plus_prefix_yields_added_style() {
        let mut panel = MainPanel::new();
        panel.push_line("+foo".into());
        assert_eq!(panel.lines.len(), 1);
        assert_eq!(panel.lines[0].style, LineStyle::Added);
        assert_eq!(panel.lines[0].text, "+foo");
    }

    // L121: -bar → LineStyle::Removed
    #[test]
    fn minus_prefix_yields_removed_style() {
        let mut panel = MainPanel::new();
        panel.push_line("-bar".into());
        assert_eq!(panel.lines.len(), 1);
        assert_eq!(panel.lines[0].style, LineStyle::Removed);
    }

    // L121: triple-backtick then `let x = 1;` → second line is LineStyle::Code
    #[test]
    fn fence_then_code_line_is_code_style() {
        let mut panel = MainPanel::new();
        panel.push_line("```".into());
        panel.push_line("let x = 1;".into());
        // The fence itself is line 0 (Code), the code line is line 1 (Code).
        assert_eq!(panel.lines.len(), 2);
        assert_eq!(panel.lines[1].style, LineStyle::Code);
    }

    // L121: scroll=5 on 10 lines — rendered lines start at index 5
    #[test]
    fn scroll_offsets_visible_range() {
        let mut panel = MainPanel::new();
        for i in 0..10u32 {
            panel.push_line(format!("line {i}"));
        }
        panel.scroll = 5;
        // The first line that would be rendered (skipping scroll) is index 5.
        let visible: Vec<&StyledLine> = panel.lines.iter().skip(panel.scroll).collect();
        assert_eq!(visible.len(), 5);
        assert_eq!(visible[0].text, "line 5");
    }

    #[test]
    fn wrap_splits_long_line_into_rows() {
        let line = Line::from("abcdefghij"); // 10 display cols
        let rows = wrap_line_to(&line, 4);
        assert_eq!(rows.len(), 3); // 4 + 4 + 2
        let joined: String = rows
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(joined, "abcdefghij");
    }

    #[test]
    fn wrap_preserves_span_style() {
        let line = Line::from(Span::styled("abcdef", Style::default().fg(Color::Red)));
        let rows = wrap_line_to(&line, 3);
        assert_eq!(rows.len(), 2);
        for r in &rows {
            for s in &r.spans {
                assert_eq!(s.style.fg, Some(Color::Red));
            }
        }
    }

    #[test]
    fn wrap_empty_line_is_single_row() {
        assert_eq!(wrap_line_to(&Line::from(String::new()), 10).len(), 1);
    }

    #[test]
    fn finalize_classifies_streamed_diff_line() {
        let mut panel = MainPanel::new();
        panel.push_line(String::new()); // tail partial line
        panel.push_delta("+added line"); // streamed text, unclassified
        panel.finalize_last_line();
        assert_eq!(panel.lines.last().unwrap().style, LineStyle::Added);
        assert_eq!(panel.lines.last().unwrap().text, "+added line");
    }

    #[test]
    fn finalize_preserves_already_styled_card() {
        let mut panel = MainPanel::new();
        panel.push_styled_line(Line::from(Span::raw("⌘ bash")));
        panel.finalize_last_line();
        let last = panel.lines.last().unwrap();
        assert_eq!(last.text, "⌘ bash");
        assert!(last.spans.is_some(), "card spans must survive finalize");
    }

    #[test]
    fn push_styled_line_backs_text_with_flattened_spans() {
        let mut panel = MainPanel::new();
        panel.push_styled_line(Line::from(vec![Span::raw("⌘ bash"), Span::raw("  find .")]));
        let last = panel.lines.last().unwrap();
        assert_eq!(last.text, "⌘ bash  find .");
        assert!(last.spans.is_some());
    }

    #[test]
    fn tool_result_meta_line_renders_dim_spans() {
        let mut panel = MainPanel::new();
        panel.push_line("↳ ok · 107 chars".into());
        assert!(panel.lines.last().unwrap().spans.is_some());
    }

    #[test]
    fn is_diff_marker_recognizes_headers_not_prose() {
        assert!(is_diff_marker("@@ -1,2 +1,3 @@"));
        assert!(is_diff_marker("diff --git a/x b/x"));
        assert!(is_diff_marker("--- a/x.rs"));
        assert!(is_diff_marker("+++ /dev/null"));
        assert!(!is_diff_marker("--- a thought --- continued"));
        assert!(!is_diff_marker("hello world"));
        assert!(!is_diff_marker("+content")); // bare +/- handled elsewhere
    }

    #[test]
    fn diff_fence_styles_hunk_and_content_with_clean_copy_text() {
        let mut panel = MainPanel::new();
        panel.push_line("```diff".into());
        panel.push_line("@@ -1,2 +1,3 @@".into());
        panel.push_line("+added".into());
        panel.push_line(" context".into());
        panel.push_line("```".into());

        let hunk = panel
            .lines
            .iter()
            .find(|l| l.text.starts_with("@@"))
            .unwrap();
        assert!(hunk.spans.is_some(), "hunk header should be styled");
        let added = panel.lines.iter().find(|l| l.text == "+added").unwrap();
        assert!(
            added.spans.is_some(),
            "added line should carry gutter spans"
        );
        // Gutter is display-only — the backing text stays clean for copy/yank.
        assert_eq!(added.text, "+added");
    }

    #[test]
    fn inline_markdown_styles_and_strips_markers() {
        // Bold/italic/code produce spans; flattened text drops the markers.
        let line = inline_markdown_spans("use **bold** and `code` here").unwrap();
        let flat: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(flat, "use bold and code here");
        // No markup → None (cheap plain path preserved).
        assert!(inline_markdown_spans("just plain prose").is_none());
        // Flanking guard: spaced asterisks (multiplication / bullets) are not italic.
        assert!(inline_markdown_spans("a * b * c").is_none());
    }

    #[test]
    fn inline_markdown_line_keeps_clean_copy_text() {
        let mut panel = MainPanel::new();
        panel.push_line("a **strong** word".into());
        let last = panel.lines.last().unwrap();
        assert!(last.spans.is_some());
        assert_eq!(last.text, "a strong word"); // markers stripped for copy
    }

    #[test]
    fn table_rows_render_cells_and_delimiter_rule() {
        assert!(is_table_row("| a | b |"));
        assert!(!is_table_row("no pipes here"));
        assert!(!is_table_row("trailing pipe a |")); // no leading pipe
        assert_eq!(
            table_cells("| a | b |"),
            vec!["a".to_owned(), "b".to_owned()]
        );

        let mut panel = MainPanel::new();
        panel.push_line("| Name | Age |".into());
        panel.push_line("|------|-----|".into());
        panel.push_line("| Ann  | 30  |".into());
        // All three rendered as styled lines; copy text stays the raw markdown.
        for sl in &panel.lines {
            assert!(sl.spans.is_some());
        }
        assert_eq!(panel.lines[0].text, "| Name | Age |");
    }

    #[test]
    fn standalone_hunk_header_is_styled() {
        let mut panel = MainPanel::new();
        panel.push_line("@@ -10,3 +10,4 @@ fn main()".into());
        assert!(panel.lines.last().unwrap().spans.is_some());
    }

    #[test]
    fn scroll_up_detaches_follow_and_bottom_rearms() {
        let mut panel = MainPanel::new();
        for i in 0..10u32 {
            panel.push_line(format!("line {i}"));
        }
        assert!(panel.follow, "streaming default is follow");
        panel.scroll_up();
        assert!(!panel.follow, "manual scroll-up detaches follow");
        for _ in 0..20 {
            panel.scroll_down();
        }
        assert!(panel.follow, "returning to the bottom re-arms follow");
    }

    #[test]
    fn render_wraps_long_line_and_hit_tests() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut panel = MainPanel::new();
        panel.push_line("x".repeat(20)); // one long logical line
        let mut term = Terminal::new(TestBackend::new(12, 8)).unwrap(); // inner width 10
        term.draw(|f| {
            let area = f.area();
            panel.render(area, f, None, None, false);
        })
        .unwrap();

        // 20 cols / inner-10 → 2 visual rows, both mapping to logical line 0.
        assert_eq!(panel.pos_at(1, 1).map(|(l, _)| l), Some(0));
        assert_eq!(panel.pos_at(1, 2).map(|(l, _)| l), Some(0));
        // Second visual row starts at char offset 10 within the logical line.
        assert_eq!(panel.pos_at(1, 2), Some((0, 10)));
        // First row, leftmost cell → char 0.
        assert_eq!(panel.pos_at(1, 1), Some((0, 0)));
        // The border cell (0,0) is outside the inner area.
        assert_eq!(panel.pos_at(0, 0), None);
    }

    #[test]
    fn selection_text_extracts_partial_and_multiline() {
        let mut panel = MainPanel::new();
        panel.push_line("hello world".into()); // line 0
        panel.push_line("second line".into()); // line 1
                                               // Partial within one line: chars [0,5) of line 0 → "hello".
        assert_eq!(panel.selection_text((0, 0), (0, 5)), "hello");
        // Order-independent.
        assert_eq!(panel.selection_text((0, 5), (0, 0)), "hello");
        // Across lines: from line0 col6 to line1 col6 → "world\nsecond".
        assert_eq!(panel.selection_text((0, 6), (1, 6)), "world\nsecond");
    }

    // L121: plain line → LineStyle::Normal
    #[test]
    fn plain_line_yields_normal_style() {
        let mut panel = MainPanel::new();
        panel.push_line("hello world".into());
        assert_eq!(panel.lines[0].style, LineStyle::Normal);
    }

    // L121: closing fence exits code mode
    #[test]
    fn closing_fence_exits_code_mode() {
        let mut panel = MainPanel::new();
        panel.push_line("```".into());
        panel.push_line("code line".into());
        panel.push_line("```".into());
        // After the closing fence, a new line should be Normal.
        panel.push_line("normal again".into());
        assert_eq!(panel.lines.last().unwrap().style, LineStyle::Normal);
    }

    // L121: language-tagged fence populates code_lang
    #[test]
    fn fence_with_lang_sets_code_lang() {
        let mut panel = MainPanel::new();
        panel.push_line("```rust".into());
        assert_eq!(panel.code_lang, "rust");
        assert!(panel.in_code_block);
    }

    // L135-L137: apply_syntect returns at least one StyledLine with non-empty text
    #[test]
    fn apply_syntect_rust_returns_nonempty_lines() {
        let lines = apply_syntect("rust", "let x = 1;");
        assert!(!lines.is_empty(), "expected at least one highlighted line");
        assert!(
            lines.iter().any(|l| !l.text.is_empty()),
            "expected at least one line with non-empty text"
        );
    }

    // L135-L137: apply_syntect with unknown lang falls back gracefully
    #[test]
    fn apply_syntect_unknown_lang_falls_back() {
        let lines = apply_syntect("xyzzy_unknown", "hello world");
        assert!(!lines.is_empty());
        assert_eq!(lines[0].style, LineStyle::Code);
    }

    // L135-L137: syntect code lines inside a rust fence are Code style
    #[test]
    fn fence_rust_code_lines_use_syntect() {
        let mut panel = MainPanel::new();
        panel.push_line("```rust".into());
        panel.push_line("fn main() {}".into());
        panel.push_line("```".into());
        // The code line (index 1) must be Code style.
        assert_eq!(panel.lines[1].style, LineStyle::Code);
    }

    // apply_syntect: rust code gets per-character RGB spans
    #[test]
    fn apply_syntect_rust_produces_rgb_spans() {
        let lines = apply_syntect("rust", "let x = 1;");
        // At least one line should have coloured spans from syntect.
        let has_spans = lines.iter().any(|l| l.spans.is_some());
        assert!(has_spans, "syntect should produce RGB spans for rust code");
    }

    // push_delta: appending two deltas to an empty panel produces one line
    #[test]
    fn push_delta_appends_to_in_progress_line() {
        let mut panel = MainPanel::new();
        panel.push_delta("hello");
        panel.push_delta(" world");
        // Should produce one line with "hello world"
        let content = panel.visible_text();
        assert!(
            content.contains("hello world"),
            "push_delta should append to same line"
        );
    }

    // push_delta: first call on an empty panel creates a new line
    #[test]
    fn push_delta_creates_new_line_when_empty() {
        let mut panel = MainPanel::new();
        panel.push_delta("first");
        let content = panel.visible_text();
        assert!(content.contains("first"));
    }

    // --- M13: word-boundary streaming flush ---

    #[test]
    fn push_delta_with_space_flushes_word() {
        let mut panel = MainPanel::new();
        panel.push_delta("word ");
        // "word " contains a space, so "word " is flushed to the committed line.
        let content = panel.visible_text();
        assert!(
            content.contains("word "),
            "visible_text should include the flushed word: {content:?}"
        );
    }

    #[test]
    fn push_delta_without_space_stays_pending() {
        let mut panel = MainPanel::new();
        panel.push_delta("word");
        // No space — pending_word holds "word"; visible_text still shows it.
        let content = panel.visible_text();
        assert!(
            content.contains("word"),
            "visible_text should include the pending word: {content:?}"
        );
    }

    #[test]
    fn push_delta_multiple_spaces_flushes_all_complete_words() {
        let mut panel = MainPanel::new();
        panel.push_delta("one two three ");
        // All complete words ("one two three ") are flushed.
        let content = panel.visible_text();
        assert!(
            content.contains("one two three "),
            "all complete words must be visible: {content:?}"
        );
    }

    #[test]
    fn scroll_down_clamps_at_last_line() {
        let mut panel = MainPanel::new();
        for i in 0..5u32 {
            panel.push_line(format!("line {i}"));
        }
        panel.scroll_to_bottom();
        let before = panel.scroll;
        panel.scroll_down();
        assert_eq!(
            panel.scroll, before,
            "scroll must not exceed last line index"
        );
    }

    #[test]
    fn scroll_up_clamps_at_zero() {
        let mut panel = MainPanel::new();
        panel.push_line("only".into());
        panel.scroll_up();
        assert_eq!(panel.scroll, 0);
    }

    #[test]
    fn scroll_to_top_sets_display_start() {
        let mut panel = MainPanel::new();
        for i in 0..5u32 {
            panel.push_line(format!("line {i}"));
        }
        panel.scroll = 4;
        panel.scroll_to_top();
        assert_eq!(panel.scroll, panel.display_start);
    }

    #[test]
    fn clear_display_advances_watermark_and_scroll() {
        let mut panel = MainPanel::new();
        for i in 0..5u32 {
            panel.push_line(format!("line {i}"));
        }
        panel.clear_display();
        assert_eq!(panel.display_start, 5);
        assert_eq!(panel.scroll, 5);
    }

    #[test]
    fn scroll_up_does_not_cross_display_start() {
        let mut panel = MainPanel::new();
        for i in 0..5u32 {
            panel.push_line(format!("line {i}"));
        }
        panel.clear_display();
        panel.push_line("after clear".into());
        panel.scroll_up();
        assert_eq!(
            panel.scroll, panel.display_start,
            "scroll must not cross the clear watermark"
        );
    }

    #[test]
    fn new_lines_after_clear_are_rendered() {
        let mut panel = MainPanel::new();
        for i in 0..3u32 {
            panel.push_line(format!("old {i}"));
        }
        panel.clear_display();
        panel.push_line("new line".into());
        // scroll == display_start == 3; new line is at index 3 → visible
        let vis: Vec<&StyledLine> = panel
            .lines
            .iter()
            .skip(panel.scroll.max(panel.display_start))
            .collect();
        assert_eq!(vis.len(), 1);
        assert_eq!(vis[0].text, "new line");
    }

    #[test]
    fn scroll_to_bottom_sets_last_index() {
        let mut panel = MainPanel::new();
        for i in 0..5u32 {
            panel.push_line(format!("line {i}"));
        }
        panel.scroll_to_bottom();
        assert_eq!(panel.scroll, 4);
    }

    #[test]
    fn lines_text_returns_inclusive_range() {
        let mut panel = MainPanel::new();
        for i in 0..5u32 {
            panel.push_line(format!("L{i}"));
        }
        let got = panel.lines_text(1, 3);
        assert_eq!(got, vec!["L1", "L2", "L3"]);
    }

    #[test]
    fn lines_text_handles_reversed_order() {
        let mut panel = MainPanel::new();
        for i in 0..5u32 {
            panel.push_line(format!("L{i}"));
        }
        let got = panel.lines_text(3, 1);
        assert_eq!(got, vec!["L1", "L2", "L3"]);
    }

    #[test]
    fn push_line_auto_scrolls_when_at_bottom() {
        let mut panel = MainPanel::new();
        // Push 5 lines — each should auto-scroll since we start at bottom.
        for i in 0..5u32 {
            panel.push_line(format!("line {i}"));
        }
        assert_eq!(
            panel.scroll, 4,
            "scroll should follow new lines when at bottom"
        );
    }

    #[test]
    fn push_line_does_not_auto_scroll_when_scrolled_up() {
        let mut panel = MainPanel::new();
        for i in 0..5u32 {
            panel.push_line(format!("line {i}"));
        }
        panel.scroll_up(); // user scrolls up → detaches bottom-follow
        let before = panel.scroll;
        panel.push_line("new line".into());
        assert_eq!(
            panel.scroll, before,
            "scroll must stay when user has scrolled up"
        );
    }

    #[test]
    fn clamp_scroll_reduces_out_of_bounds_scroll() {
        let mut panel = MainPanel::new();
        for i in 0..3u32 {
            panel.push_line(format!("line {i}"));
        }
        panel.scroll = 100;
        panel.clamp_scroll();
        assert_eq!(panel.scroll, 2);
    }

    // --- math rendering -------------------------------------------------------

    #[test]
    fn render_math_converts_inline_greek() {
        let out = render_math("$\\alpha + \\beta$");
        assert_eq!(
            out, "α + β",
            "Greek letters must be converted; got: {out:?}"
        );
    }

    #[test]
    fn render_math_converts_superscript_digit() {
        let out = render_math("$E = mc^2$");
        assert_eq!(out, "E = mc²", "^2 must become ²; got: {out:?}");
    }

    #[test]
    fn render_math_converts_subscript_digit() {
        let out = render_math("$x_0$");
        assert_eq!(out, "x₀", "subscript 0 must become ₀; got: {out:?}");
    }

    #[test]
    fn render_math_leaves_unknown_commands_verbatim() {
        let out = render_math("$\\unknowncmd$");
        assert!(
            out.contains("\\unknowncmd"),
            "unrecognised commands must be passed through; got: {out:?}"
        );
    }

    #[test]
    fn render_math_does_not_touch_text_outside_dollars() {
        let out = render_math("no math here");
        assert_eq!(out, "no math here");
    }

    #[test]
    fn render_math_dollar_sign_without_closing_delimiter_is_unchanged() {
        let out = render_math("cost is $5 per month");
        // "5 per month" has no closing $, so the $ should be preserved
        assert!(
            out.contains('$') || out.contains("5 per month"),
            "unclosed $ must not panic; got: {out:?}"
        );
    }

    #[test]
    fn push_line_applies_math_rendering() {
        let mut panel = MainPanel::new();
        panel.push_line("$\\pi$ is about 3.14".to_owned());
        assert!(
            panel.lines[0].text.contains('π'),
            "push_line must apply math rendering; got: {:?}",
            panel.lines[0].text
        );
    }

    #[test]
    fn push_seam_inserts_divider_with_token_count() {
        let mut panel = MainPanel::new();
        panel.push_seam(512);
        assert_eq!(panel.lines.len(), 1);
        let line = &panel.lines[0];
        assert!(line.text.contains('↩'), "seam must contain return arrow");
        assert!(line.text.contains("512"), "seam must include token count");
        assert!(line.text.contains("summarized"), "seam must say summarized");
        // Pre-built styled spans must be present (seam uses spans directly)
        assert!(line.spans.is_some(), "seam line must have pre-built spans");
    }

    #[test]
    fn push_seam_without_tokens_omits_count() {
        let mut panel = MainPanel::new();
        panel.push_seam(0);
        assert!(
            !panel.lines[0].text.contains("0 tok"),
            "zero tokens must not show count"
        );
        assert!(panel.lines[0].text.contains("summarized"));
    }

    // -------------------------------------------------------------------------
    // M3 — Language auto-detection for unlabeled fenced code blocks
    // -------------------------------------------------------------------------

    #[test]
    fn detect_lang_rust_fn_main() {
        assert_eq!(detect_lang(&["fn main() {"]), "rust");
    }

    #[test]
    fn detect_lang_python_shebang() {
        assert_eq!(
            detect_lang(&["#!/usr/bin/python", "print('hello')"]),
            "python"
        );
    }

    #[test]
    fn detect_lang_bash_shebang() {
        assert_eq!(detect_lang(&["#!/bin/bash", "echo hi"]), "bash");
    }

    #[test]
    fn detect_lang_go_package() {
        assert_eq!(detect_lang(&["package main", "func main() {"]), "go");
    }

    #[test]
    fn detect_lang_sql_select() {
        assert_eq!(detect_lang(&["SELECT id FROM users"]), "sql");
    }

    #[test]
    fn detect_lang_unknown_falls_back() {
        assert_eq!(detect_lang(&["hello world"]), "");
    }

    #[test]
    fn push_line_unlabeled_rust_block_detected() {
        let mut panel = MainPanel::new();
        // Opening fence with no language.
        panel.push_line("```".into());
        panel.push_line("fn main() {}".into());
        // Closing fence — detection fires here.
        panel.push_line("```".into());
        // The code line (index 1) should be syntax-highlighted (spans present).
        assert!(
            panel.lines[1].spans.is_some(),
            "unlabeled rust block must be retroactively highlighted"
        );
    }

    // --- M15: word-level diff highlighting ---

    #[test]
    fn diff_line_spans_added_has_word_styling() {
        // A line with an inserted word should produce spans — some styled.
        let line = word_diff_spans("hello world", "hello Rust world");
        // There must be more than one span (unchanged + changed portions).
        assert!(
            line.spans.len() > 1,
            "word diff should produce multiple spans, got: {:?}",
            line.spans
        );
        // At least one span must have a non-default fg colour (the added word).
        let has_styled = line.spans.iter().any(|s| s.style.fg.is_some());
        assert!(
            has_styled,
            "at least one span must be styled for the added word"
        );
    }

    // --- M16: cell spacing modes ---

    #[test]
    fn compact_spacing_no_blank_rows_between_lines() {
        let spacing = Spacing::Compact;
        // A chip line (spans present) in Compact mode produces 0 blank rows.
        assert_eq!(
            spacing.blank_rows_after_chip(),
            0,
            "Compact must not insert any blank rows"
        );
    }

    #[test]
    fn comfortable_spacing_inserts_one_blank_row_after_chip() {
        let spacing = Spacing::Comfortable;
        assert_eq!(
            spacing.blank_rows_after_chip(),
            1,
            "Comfortable must insert one blank row after a chip"
        );
    }

    #[test]
    fn spacious_spacing_inserts_two_blank_rows_after_chip() {
        let spacing = Spacing::Spacious;
        assert_eq!(spacing.blank_rows_after_chip(), 2);
    }

    #[test]
    fn panel_default_spacing_is_comfortable() {
        let panel = MainPanel::new();
        assert_eq!(panel.spacing, Spacing::Comfortable);
    }

    #[test]
    fn similar_word_diff_finds_changed_word() {
        let line = word_diff_spans("foo bar baz", "foo qux baz");
        // "bar" should be marked deleted and "qux" should be marked inserted.
        let deleted: Vec<_> = line
            .spans
            .iter()
            .filter(|s| {
                s.style.fg.is_some() && s.style.add_modifier.contains(Modifier::CROSSED_OUT)
            })
            .collect();
        let inserted: Vec<_> = line
            .spans
            .iter()
            .filter(|s| {
                s.style.fg.is_some() && !s.style.add_modifier.contains(Modifier::CROSSED_OUT)
            })
            .collect();
        assert!(
            !deleted.is_empty(),
            "deleted word must be present: {:?}",
            line.spans
        );
        assert!(
            !inserted.is_empty(),
            "inserted word must be present: {:?}",
            line.spans
        );
    }

    // --- M21: image detection and placeholder rendering ---

    #[test]
    fn detect_image_finds_data_uri() {
        let line = "Here is an image: data:image/png;base64,abc123 and some text";
        let src = detect_image_in_line(line);
        assert!(src.is_some(), "data URI must be detected");
        assert!(
            src.unwrap().starts_with("data:image/"),
            "src must start with data:image/"
        );
    }

    #[test]
    fn detect_image_finds_png_path() {
        let line = "Screenshot saved to /tmp/shot.png for reference";
        let src = detect_image_in_line(line);
        assert!(src.is_some(), "png path must be detected: {line}");
        assert!(
            std::path::Path::new(src.unwrap())
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("png")),
            "detected src must end with .png"
        );
    }

    #[test]
    fn render_image_placeholder_format() {
        let placeholder = render_image_placeholder("data:image/png;base64,abc");
        assert_eq!(placeholder, "[image: data:image/png;base64,abc]");
    }

    #[test]
    fn no_image_in_plain_text() {
        let line = "This is plain text with no image reference.";
        assert!(
            detect_image_in_line(line).is_none(),
            "plain text must not trigger image detection"
        );
    }
}
