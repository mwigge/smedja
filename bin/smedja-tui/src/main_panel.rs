//! `MainPanel` widget — scrollable message area with diff-aware line styling.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

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
    fn plain(text: String, style: LineStyle) -> Self {
        Self { text, style, spans: None }
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
    /// Whether the next pushed line should be treated as code.
    in_code_block: bool,
    /// Language tag from the opening fence (e.g. "rust"), empty if none.
    code_lang: String,
}

impl MainPanel {
    /// Creates a new, empty [`MainPanel`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            scroll: 0,
            display_start: 0,
            in_code_block: false,
            code_lang: String::new(),
        }
    }

    /// Advances the display watermark to the current line count, hiding all
    /// previously pushed lines without discarding them.  Used by `/clear`.
    pub fn clear_display(&mut self) {
        self.display_start = self.lines.len();
        self.scroll = self.lines.len();
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
        // Track whether we're at the bottom before pushing so we can auto-scroll.
        let was_at_bottom = self.scroll + 1 >= self.lines.len();

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
            if self.code_lang.is_empty() {
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
        } else {
            // Outside code blocks: classify by prefix.
            let style = if text.starts_with('+') {
                LineStyle::Added
            } else if text.starts_with('-') {
                LineStyle::Removed
            } else {
                LineStyle::Normal
            };
            self.lines.push(StyledLine::plain(text, style));
        }

        if was_at_bottom {
            self.scroll = self.lines.len().saturating_sub(1);
        }
    }

    /// Renders the panel into `frame` at `area`, respecting the scroll offset.
    ///
    /// `selection` highlights lines from `lo` to `hi` (inclusive) in reverse video.
    /// `search_query` highlights lines containing the query text (yellow background).
    /// `no_color` strips all colours when the `NO_COLOR` env var is set.
    pub fn render(
        &self,
        area: Rect,
        frame: &mut Frame,
        selection: Option<(usize, usize)>,
        search_query: Option<&str>,
        no_color: bool,
    ) {
        let height = area.height.saturating_sub(2) as usize; // subtract border rows

        let search_needle = search_query
            .filter(|q| !q.is_empty())
            .map(|q| q.to_lowercase());

        let visible: Vec<Line<'_>> = self
            .lines
            .iter()
            .enumerate()
            .skip(self.scroll.max(self.display_start))
            .take(height)
            .map(|(abs_line, sl)| {
                let selected =
                    selection.is_some_and(|(lo, hi)| abs_line >= lo && abs_line <= hi);
                let is_search_match = search_needle
                    .as_deref()
                    .is_some_and(|q| sl.text.to_lowercase().contains(q));

                if selected {
                    // Selection always overrides spans — flatten to a single styled span.
                    let text = sl
                        .spans
                        .as_ref()
                        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
                        .unwrap_or_else(|| sl.text.clone());
                    Line::from(Span::styled(
                        text,
                        Style::default().fg(Color::Black).bg(Color::White),
                    ))
                } else if is_search_match {
                    // Search match: yellow highlight overrides normal rendering.
                    Line::from(Span::styled(
                        sl.text.clone(),
                        Style::default().fg(Color::Black).bg(Color::Yellow),
                    ))
                } else if let Some(ref rich) = sl.spans {
                    if no_color {
                        let text =
                            rich.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
                        Line::raw(text)
                    } else {
                        rich.clone()
                    }
                } else {
                    let base = if no_color {
                        Style::default()
                    } else {
                        match sl.style {
                            LineStyle::Normal  => Style::default(),
                            LineStyle::Added   => Style::default().fg(Color::Green),
                            LineStyle::Removed => Style::default().fg(Color::Red),
                            LineStyle::Code    => Style::default().fg(Color::Yellow),
                        }
                    };
                    Line::from(Span::styled(sl.text.clone(), base))
                }
            })
            .collect();

        let widget = Paragraph::new(visible)
            .block(Block::default().borders(Borders::ALL).title("messages"));
        frame.render_widget(widget, area);
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
        self.scroll = self.scroll.saturating_sub(1).max(self.display_start);
    }

    pub fn scroll_down(&mut self) {
        let max = self.lines.len().saturating_sub(1);
        if self.scroll < max {
            self.scroll += 1;
        }
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = self.display_start;
    }

    pub fn scroll_to_bottom(&mut self) {
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
            self.lines.push(StyledLine::plain(text.to_owned(), LineStyle::Normal));
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
// Syntect integration
// ---------------------------------------------------------------------------

/// Highlights `code` for the given `lang` using syntect, returning one
/// [`StyledLine`] per input line with per-character [`Color::Rgb`] spans.
///
/// Falls back to plain [`LineStyle::Code`] lines when the language is unknown
/// or highlighting fails.
#[must_use]
pub fn apply_syntect(lang: &str, code: &str) -> Vec<StyledLine> {
    let ss = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();

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
        match highlighter.highlight_line(line, &ss) {
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
        assert_eq!(panel.scroll, 4, "scroll should follow new lines when at bottom");
    }

    #[test]
    fn push_line_does_not_auto_scroll_when_scrolled_up() {
        let mut panel = MainPanel::new();
        for i in 0..5u32 {
            panel.push_line(format!("line {i}"));
        }
        panel.scroll = 0; // user scrolled up
        panel.push_line("new line".into());
        assert_eq!(panel.scroll, 0, "scroll must stay when user has scrolled up");
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
}
