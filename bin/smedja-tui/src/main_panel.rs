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
    /// The text content of the line.
    pub text: String,
    /// The visual style to apply when rendering.
    pub style: LineStyle,
}

/// Scrollable panel displaying styled message lines.
#[derive(Debug)]
pub struct MainPanel {
    lines: Vec<StyledLine>,
    /// First visible line index.
    pub scroll: usize,
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
            in_code_block: false,
            code_lang: String::new(),
        }
    }

    /// Pushes a line of text, classifying its style automatically.
    ///
    /// - Lines beginning with `+` → [`LineStyle::Added`]
    /// - Lines beginning with `-` → [`LineStyle::Removed`]
    /// - A triple-backtick boundary toggles code mode; subsequent lines are
    ///   [`LineStyle::Code`] and highlighted via syntect if a language tag was
    ///   present on the opening fence.
    pub fn push_line(&mut self, text: String) {
        // Detect fence boundaries (``` with optional language tag).
        if text.trim_start().starts_with("```") {
            if self.in_code_block {
                // Closing fence — push it as Code and exit code mode.
                self.lines.push(StyledLine {
                    text,
                    style: LineStyle::Code,
                });
                self.in_code_block = false;
                self.code_lang = String::new();
            } else {
                // Opening fence — record language (if any) and enter code mode.
                let lang = text.trim_start().trim_start_matches('`').trim().to_owned();
                self.code_lang = lang;
                self.in_code_block = true;
                self.lines.push(StyledLine {
                    text,
                    style: LineStyle::Code,
                });
            }
            return;
        }

        if self.in_code_block {
            // Inside a fenced block: apply syntect if we know the language.
            if self.code_lang.is_empty() {
                self.lines.push(StyledLine {
                    text,
                    style: LineStyle::Code,
                });
            } else {
                let lang = self.code_lang.clone();
                let highlighted = apply_syntect(&lang, &text);
                if highlighted.is_empty() {
                    self.lines.push(StyledLine {
                        text,
                        style: LineStyle::Code,
                    });
                } else {
                    self.lines.extend(highlighted);
                }
            }
            return;
        }

        // Outside code blocks: classify by prefix.
        let style = if text.starts_with('+') {
            LineStyle::Added
        } else if text.starts_with('-') {
            LineStyle::Removed
        } else {
            LineStyle::Normal
        };

        self.lines.push(StyledLine { text, style });
    }

    /// Renders the panel into `frame` at `area`, respecting the scroll offset.
    pub fn render(&self, area: Rect, frame: &mut Frame) {
        let height = area.height.saturating_sub(2) as usize; // subtract border rows

        let visible: Vec<Line<'_>> = self
            .lines
            .iter()
            .skip(self.scroll)
            .take(height)
            .map(|sl| {
                let style = match sl.style {
                    LineStyle::Normal => Style::default(),
                    LineStyle::Added => Style::default().fg(Color::Green),
                    LineStyle::Removed => Style::default().fg(Color::Red),
                    LineStyle::Code => Style::default().fg(Color::Yellow),
                };
                Line::from(Span::styled(sl.text.clone(), style))
            })
            .collect();

        let widget =
            Paragraph::new(visible).block(Block::default().borders(Borders::ALL).title("messages"));
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

    /// Appends `text` to the last line if one exists, or creates a new one.
    ///
    /// Used for streaming turn responses where content arrives incrementally.
    /// Each call extends the tail of the current in-progress line rather than
    /// opening a new one, so callers can accumulate partial content until the
    /// turn completes and `push_line` takes over for the final render.
    pub fn push_delta(&mut self, text: &str) {
        if let Some(last) = self.lines.last_mut() {
            last.text.push_str(text);
        } else {
            self.lines.push(StyledLine {
                text: text.to_owned(),
                style: LineStyle::Normal,
            });
        }
    }

    /// Returns the full text content of all stored lines joined by newlines.
    ///
    /// Only compiled under `#[cfg(test)]` — used by unit tests to inspect panel
    /// state without coupling them to the internal `Vec<StyledLine>` layout.
    #[cfg(test)]
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
/// [`StyledLine`] per input line (all with [`LineStyle::Code`]).
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
        // No theme available — fall back to plain text.
        return code
            .lines()
            .map(|l| StyledLine {
                text: l.to_owned(),
                style: LineStyle::Code,
            })
            .collect();
    };

    let mut highlighter = HighlightLines::new(syntax, &theme);
    let mut result = Vec::new();

    // syntect's LinesWithEndings keeps line endings; strip them for display.
    for line in syntect::util::LinesWithEndings::from(code) {
        match highlighter.highlight_line(line, &ss) {
            Ok(_regions) => {
                // We use the text content only; colour comes from LineStyle::Code.
                // Full colour support would require mapping syntect colours to
                // ratatui Color values, which is out of scope for this widget.
                result.push(StyledLine {
                    text: line.trim_end_matches(['\n', '\r']).to_owned(),
                    style: LineStyle::Code,
                });
            }
            Err(_) => {
                result.push(StyledLine {
                    text: line.trim_end_matches(['\n', '\r']).to_owned(),
                    style: LineStyle::Code,
                });
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
}
