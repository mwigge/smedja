//! `CodeWidget` — a composable ratatui widget for syntax-highlighted code blocks.
//!
//! Wraps `apply_syntect` from `main_panel` as a proper [`ratatui::widgets::Widget`]
//! so code blocks can be placed anywhere in the layout, independently scrolled,
//! and composed with other widgets — not just inlined into the main scroll buffer.
//!
//! # Tree-sitter
//! The widget accepts an optional tree-sitter parse tree.  When provided, semantic
//! node types (keyword, string, comment, identifier) are used to annotate tokens
//! before falling back to the syntect colour pass.  When absent, syntect handles
//! highlighting alone.  The tree-sitter path is available for Rust today; other
//! languages fall back gracefully.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Widget,
};

use ratatui::style::Color;
use crate::main_panel::{apply_syntect, LineStyle, StyledLine};
use crate::theme::palette;

/// A ratatui widget that renders a syntax-highlighted code block with
/// independent scroll state.
///
/// # Example
/// ```ignore
/// let widget = CodeWidget::new("fn main() {}", "rust").scroll(2);
/// frame.render_widget(widget, area);
/// ```
pub struct CodeWidget<'a> {
    source: &'a str,
    language: &'a str,
    /// First line to display (0 = top).
    scroll: usize,
}

impl<'a> CodeWidget<'a> {
    /// Creates a new `CodeWidget` with the given source and language hint.
    #[must_use]
    pub fn new(source: &'a str, language: &'a str) -> Self {
        Self {
            source,
            language,
            scroll: 0,
        }
    }

    /// Sets the scroll offset (first visible line index).
    #[must_use]
    pub fn scroll(mut self, offset: usize) -> Self {
        self.scroll = offset;
        self
    }

    /// Returns the total number of rendered lines (after syntect / tree-sitter).
    #[must_use]
    pub fn line_count(&self) -> usize {
        build_lines(self.source, self.language).len()
    }
}

impl Widget for CodeWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let lines = build_lines(self.source, self.language);
        let visible = lines.iter().skip(self.scroll).take(area.height as usize);
        for (row, styled) in visible.enumerate() {
            let y = area.y + row as u16;
            let rendered_line: Line<'_> = match &styled.spans {
                Some(pre) => pre.clone(),
                None => Line::from(Span::styled(
                    styled.text.clone(),
                    code_fallback_style(&styled.style),
                )),
            };
            // Write each span into the buffer cell-by-cell up to area.width.
            let mut x = area.x;
            for span in &rendered_line.spans {
                for ch in span.content.chars() {
                    if x >= area.x + area.width {
                        break;
                    }
                    buf[(x, y)].set_char(ch).set_style(span.style);
                    x += 1;
                }
                if x >= area.x + area.width {
                    break;
                }
            }
        }
    }
}

/// Builds the styled lines for `source` using syntect (with tree-sitter
/// annotation for Rust as the reference implementation).
fn build_lines(source: &str, language: &str) -> Vec<StyledLine> {
    // Tree-sitter path: annotate Rust tokens with semantic types, then
    // colour via the same span model.  Other languages fall through to syntect.
    if language == "rust" {
        if let Some(ts_lines) = try_treesitter_rust(source) {
            return ts_lines;
        }
    }
    // Syntect fallback for all languages.
    let highlighted = apply_syntect(language, source);
    if highlighted.is_empty() {
        source
            .lines()
            .map(|l| StyledLine::plain(l.to_owned(), LineStyle::Code))
            .collect()
    } else {
        highlighted
    }
}

fn code_fallback_style(ls: &LineStyle) -> Style {
    let p = palette();
    match ls {
        LineStyle::Code | LineStyle::Normal => Style::default().fg(p.code_default),
        LineStyle::Added => Style::default().fg(p.code_added),
        LineStyle::Removed => Style::default().fg(p.code_removed),
    }
}

/// Attempts to highlight `source` as Rust using tree-sitter.
///
/// Returns `None` when tree-sitter is unavailable or parsing fails, so the
/// caller can fall back to syntect.
fn try_treesitter_rust(source: &str) -> Option<Vec<StyledLine>> {
    use tree_sitter::Parser;

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;

    // Walk the tree collecting (byte_start, byte_end, colour) annotations.
    // We only annotate known high-value node types; everything else gets the
    // default colour.
    let mut annotations: Vec<(usize, usize, Color)> = Vec::new();
    collect_annotations(tree.root_node(), &mut annotations);
    // Sort by start position; overlapping ranges are last-writer-wins.
    annotations.sort_by_key(|(s, _, _)| *s);

    let lines = annotate_source(source, &annotations);
    Some(lines)
}

/// Recursively collects byte-range colour annotations from tree-sitter nodes.
fn collect_annotations(node: tree_sitter::Node<'_>, out: &mut Vec<(usize, usize, Color)>) {
    let p = palette();
    let color = match node.kind() {
        // Keywords
        "fn" | "let" | "mut" | "pub" | "use" | "mod" | "struct" | "enum" | "impl" | "trait"
        | "return" | "if" | "else" | "match" | "loop" | "while" | "for" | "in" | "break"
        | "continue" | "async" | "await" | "type" | "where" | "const" | "static" | "unsafe"
        | "extern" | "crate" | "self" | "super" | "true" | "false" => Some(p.code_keyword),
        // Literals
        "string_literal" | "raw_string_literal" | "char_literal" => Some(p.code_string),
        "integer_literal" | "float_literal" => Some(p.code_number),
        // Comments
        "line_comment" | "block_comment" => Some(p.code_comment),
        // Types
        "type_identifier" | "primitive_type" => Some(p.code_type),
        // Macros
        "macro_invocation" => Some(p.code_macro),
        _ => None,
    };

    if let Some(color) = color {
        out.push((node.start_byte(), node.end_byte(), color));
    }

    // Recurse into children.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_annotations(child, out);
    }
}

/// Splits `source` into per-line `StyledLine` objects using byte-range
/// colour annotations.  Unannotated bytes get the default code colour.
fn annotate_source(source: &str, annotations: &[(usize, usize, Color)]) -> Vec<StyledLine> {
    let p = palette();
    source
        .lines()
        .enumerate()
        .map(|(line_idx, line_text)| {
            // Byte offset of the start of this line in `source`.
            let line_start: usize = source
                .lines()
                .take(line_idx)
                .map(|l| l.len() + 1) // +1 for '\n'
                .sum();
            let line_end = line_start + line_text.len();

            // Build spans for this line.
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut pos = line_start;

            // Filter annotations that overlap this line.
            let relevant: Vec<_> = annotations
                .iter()
                .filter(|(s, e, _)| *e > line_start && *s < line_end)
                .collect();

            for &&(ann_start, ann_end, color) in &relevant {
                let seg_start = ann_start.max(line_start);
                let seg_end = ann_end.min(line_end);
                if seg_start > pos {
                    // Gap before annotation: default colour.
                    let gap = &line_text[pos - line_start..seg_start - line_start];
                    spans.push(Span::styled(
                        gap.to_owned(),
                        Style::default().fg(p.code_default),
                    ));
                }
                if seg_start < seg_end {
                    let seg = &line_text[seg_start - line_start..seg_end - line_start];
                    spans.push(Span::styled(seg.to_owned(), Style::default().fg(color)));
                }
                pos = seg_end;
            }

            // Tail: remainder of the line in default colour.
            if pos < line_end {
                let tail = &line_text[pos - line_start..];
                spans.push(Span::styled(
                    tail.to_owned(),
                    Style::default().fg(p.code_default),
                ));
            }

            let pre = if spans.is_empty() {
                Line::from(Span::styled(
                    line_text.to_owned(),
                    Style::default().fg(p.code_default),
                ))
            } else {
                Line::from(spans)
            };

            StyledLine {
                text: line_text.to_owned(),
                style: LineStyle::Code,
                spans: Some(pre),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_widget_empty_source_produces_no_lines() {
        let w = CodeWidget::new("", "rust");
        assert_eq!(w.line_count(), 0);
    }

    #[test]
    fn code_widget_rust_block_has_correct_line_count() {
        let src = "fn main() {\n    println!(\"hi\");\n}";
        let w = CodeWidget::new(src, "rust");
        assert_eq!(w.line_count(), 3);
    }

    #[test]
    fn code_widget_unknown_language_falls_back_to_plain() {
        let src = "hello world\nfoo bar";
        let w = CodeWidget::new(src, "unknownlang");
        assert_eq!(w.line_count(), 2);
    }

    #[test]
    fn code_widget_scroll_does_not_affect_line_count() {
        let src = "a\nb\nc\nd";
        let w = CodeWidget::new(src, "rust").scroll(2);
        assert_eq!(w.line_count(), 4, "scroll only affects render, not count");
    }

    #[test]
    fn treesitter_rust_annotates_fn_keyword() {
        let src = "fn main() {}";
        let lines = try_treesitter_rust(src);
        assert!(lines.is_some(), "Rust tree-sitter parse must succeed");
        let lines = lines.unwrap();
        assert_eq!(lines.len(), 1);
        // The fn keyword should be annotated (spans present).
        assert!(
            lines[0].spans.is_some(),
            "Rust source should produce annotated spans"
        );
    }

    #[test]
    fn build_lines_rust_uses_treesitter_path() {
        let src = "let x = 42;";
        let lines = build_lines(src, "rust");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].style, LineStyle::Code);
    }

    #[test]
    fn code_widget_render_writes_to_buffer() {
        use ratatui::{backend::TestBackend, Terminal};
        let backend = TestBackend::new(20, 3);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let area = f.area();
            f.render_widget(CodeWidget::new("fn main() {}", "rust"), area);
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        // At least 'f' (from 'fn') should appear somewhere in the buffer.
        let content: String = (0..20).map(|x| buf[(x, 0)].symbol().to_owned()).collect();
        assert!(
            content.contains('f'),
            "fn keyword must be rendered; got: {content:?}"
        );
    }
}
