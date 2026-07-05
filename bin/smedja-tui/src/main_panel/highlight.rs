//! Syntax highlighting for fenced code blocks: a tree-sitter dispatch for a
//! handful of languages with a cached syntect fallback, plus scope/node to
//! forge-palette colour mapping. Moved verbatim from `main_panel.rs`;
//! `super` is the panel module.

use super::{LineStyle, StyledLine};
use crate::theme::{palette, Palette};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::sync::OnceLock;
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

// ---------------------------------------------------------------------------
// Syntax highlighting — tree-sitter dispatch + cached syntect fallback
// ---------------------------------------------------------------------------

/// The default syntect [`SyntaxSet`], loaded once and reused across all calls.
///
/// `load_defaults_newlines` is comparatively expensive (parses every bundled
/// syntax definition), so caching it in a [`OnceLock`] avoids rebuilding it on
/// every code block.
static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Highlights `code` for `lang`, preferring the tree-sitter path (rust, go,
/// python, typescript) and falling back to the cached syntect classifier for
/// every other language.  Returns one [`StyledLine`] per input line.
///
/// Both paths honour the runtime forge `CODE_*` palette, so syntax colours track
/// `[tui.colors]` overrides regardless of which backend produced them.
#[must_use]
pub(crate) fn highlight_code(lang: &str, code: &str) -> Vec<StyledLine> {
    if let Some(l) = ts_lang(lang) {
        if let Some(lines) = highlight_treesitter(l, code) {
            return lines;
        }
    }
    apply_syntect(lang, code)
}

/// Highlights `code` for the given `lang` using the cached syntect
/// [`SyntaxSet`], classifying each token's scope into a forge `CODE_*` colour
/// (no external theme — colours come straight from the runtime [`palette`]).
///
/// Falls back to plain [`LineStyle::Code`] lines when highlighting fails.
#[must_use]
pub(crate) fn apply_syntect(lang: &str, code: &str) -> Vec<StyledLine> {
    let ss = syntax_set();
    let p = palette();

    let syntax = ss
        .find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_extension(lang))
        .unwrap_or_else(|| ss.find_syntax_plain_text());

    let mut state = ParseState::new(syntax);
    let mut result = Vec::new();

    for line in syntect::util::LinesWithEndings::from(code) {
        let text = line.trim_end_matches(['\n', '\r']).to_owned();
        let spans: Vec<Span<'static>> = match state.parse_line(line, ss) {
            Ok(ops) => {
                let mut stack = ScopeStack::new();
                let mut spans: Vec<Span<'static>> = Vec::new();
                let mut last = 0usize;
                for (i, op) in ops {
                    if let Some(span) = scope_span(line, last, i, &stack, p) {
                        spans.push(span);
                    }
                    let _ = stack.apply(&op);
                    last = i;
                }
                if let Some(span) = scope_span(line, last, line.len(), &stack, p) {
                    spans.push(span);
                }
                spans
            }
            Err(_) => Vec::new(),
        };
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

    result
}

/// Builds a styled span for the `line[from..to]` byte range, colouring it by the
/// current scope stack.  Returns `None` for empty / whitespace-only trailing
/// ranges so blank tails do not emit spans.
fn scope_span(
    line: &str,
    from: usize,
    to: usize,
    stack: &ScopeStack,
    p: &Palette,
) -> Option<Span<'static>> {
    if to <= from {
        return None;
    }
    let seg = line.get(from..to)?.trim_end_matches(['\n', '\r']);
    if seg.is_empty() {
        return None;
    }
    let color = scope_to_forge_color(stack, p).unwrap_or(p.code_default);
    Some(Span::styled(seg.to_owned(), Style::default().fg(color)))
}

/// Classifies the deepest syntect scope on `stack` into a forge `CODE_*` colour.
///
/// Uses textmate scope-name substring matching (most-specific category first).
/// Returns `None` for unclassified scopes so the caller applies `code_default`.
fn scope_to_forge_color(stack: &ScopeStack, p: &Palette) -> Option<Color> {
    // `ScopeStack`'s Display prints the space-separated dotted scope path.
    let scopes = format!("{stack}");
    if scopes.contains("comment") {
        Some(p.code_comment)
    } else if scopes.contains("string") {
        Some(p.code_string)
    } else if scopes.contains("constant.numeric") {
        Some(p.code_number)
    } else if scopes.contains("entity.name.macro") {
        Some(p.code_macro)
    } else if scopes.contains("entity.name.type")
        || scopes.contains("support.type")
        || scopes.contains("storage.type")
    {
        Some(p.code_type)
    } else if scopes.contains("entity.name.function") || scopes.contains("support.function") {
        Some(p.code_default)
    } else if scopes.contains("keyword") || scopes.contains("storage.modifier") {
        Some(p.code_keyword)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tree-sitter highlighting (rust / go / python / typescript)
// ---------------------------------------------------------------------------

/// Languages with a dedicated tree-sitter grammar in the highlighter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TsLang {
    Rust,
    Go,
    Python,
    TypeScript,
}

/// Maps a language tag (and common aliases) to a [`TsLang`], or `None` when no
/// tree-sitter grammar is wired for it (caller falls back to syntect).
pub(crate) fn ts_lang(lang: &str) -> Option<TsLang> {
    match lang.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" => Some(TsLang::Rust),
        "go" | "golang" => Some(TsLang::Go),
        "py" | "python" => Some(TsLang::Python),
        "ts" | "tsx" | "typescript" => Some(TsLang::TypeScript),
        _ => None,
    }
}

/// Returns the tree-sitter [`Language`](tree_sitter::Language) for a [`TsLang`].
fn ts_language(l: TsLang) -> tree_sitter::Language {
    match l {
        TsLang::Rust => tree_sitter_rust::LANGUAGE.into(),
        TsLang::Go => tree_sitter_go::LANGUAGE.into(),
        TsLang::Python => tree_sitter_python::LANGUAGE.into(),
        TsLang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
    }
}

/// Maps a grammar node kind to a forge `CODE_*` colour for the given language.
///
/// Uses each grammar's real node kinds (anonymous keyword tokens like `"func"`
/// or `"def"`, plus named literal/comment/type kinds).  Returns `None` for kinds
/// that should keep the default code colour.
fn node_color(l: TsLang, kind: &str, p: &Palette) -> Option<Color> {
    match l {
        TsLang::Rust => match kind {
            "fn" | "let" | "mut" | "pub" | "use" | "mod" | "struct" | "enum" | "impl" | "trait"
            | "return" | "if" | "else" | "match" | "loop" | "while" | "for" | "in" | "break"
            | "continue" | "async" | "await" | "type" | "where" | "const" | "static" | "unsafe"
            | "extern" | "crate" | "self" | "super" | "true" | "false" | "move" | "ref" | "dyn"
            | "as" => Some(p.code_keyword),
            "string_literal" | "raw_string_literal" | "char_literal" => Some(p.code_string),
            "integer_literal" | "float_literal" => Some(p.code_number),
            "line_comment" | "block_comment" => Some(p.code_comment),
            "type_identifier" | "primitive_type" => Some(p.code_type),
            "macro_invocation" => Some(p.code_macro),
            _ => None,
        },
        TsLang::Go => match kind {
            "func" | "package" | "import" | "return" | "type" | "struct" | "interface" | "if"
            | "else" | "for" | "range" | "var" | "const" | "map" | "go" | "defer" | "chan"
            | "select" | "switch" | "case" | "default" | "break" | "continue" | "fallthrough"
            | "goto" | "true" | "false" | "nil" | "iota" => Some(p.code_keyword),
            "interpreted_string_literal" | "raw_string_literal" | "rune_literal" => {
                Some(p.code_string)
            }
            "int_literal" | "float_literal" | "imaginary_literal" => Some(p.code_number),
            "comment" => Some(p.code_comment),
            "type_identifier" => Some(p.code_type),
            _ => None,
        },
        TsLang::Python => match kind {
            "def" | "class" | "return" | "if" | "elif" | "else" | "for" | "while" | "import"
            | "from" | "as" | "with" | "try" | "except" | "finally" | "raise" | "pass"
            | "break" | "continue" | "lambda" | "yield" | "global" | "nonlocal" | "assert"
            | "del" | "in" | "is" | "not" | "and" | "or" | "async" | "await" | "true" | "false"
            | "none" => Some(p.code_keyword),
            "string" | "concatenated_string" => Some(p.code_string),
            "integer" | "float" => Some(p.code_number),
            "comment" => Some(p.code_comment),
            _ => None,
        },
        TsLang::TypeScript => match kind {
            "function" | "const" | "let" | "var" | "class" | "interface" | "return" | "import"
            | "export" | "type" | "if" | "else" | "for" | "while" | "new" | "extends"
            | "implements" | "public" | "private" | "protected" | "readonly" | "static"
            | "async" | "await" | "yield" | "enum" | "namespace" | "declare" | "as" | "from"
            | "of" | "in" | "instanceof" | "typeof" | "true" | "false" | "null" | "undefined" => {
                Some(p.code_keyword)
            }
            "string" | "template_string" => Some(p.code_string),
            "number" => Some(p.code_number),
            "comment" => Some(p.code_comment),
            "type_identifier" | "predefined_type" => Some(p.code_type),
            _ => None,
        },
    }
}

/// Highlights `code` with the tree-sitter grammar for `l`.
///
/// Returns `None` when the parser cannot be constructed, so the caller can fall
/// back to syntect.
fn highlight_treesitter(l: TsLang, code: &str) -> Option<Vec<StyledLine>> {
    use tree_sitter::Parser;

    let p = palette();
    let mut parser = Parser::new();
    parser.set_language(&ts_language(l)).ok()?;
    let tree = parser.parse(code, None)?;

    let mut annotations: Vec<(usize, usize, Color)> = Vec::new();
    collect_annotations(l, tree.root_node(), p, &mut annotations);
    // Sort by start position; overlapping ranges are last-writer-wins.
    annotations.sort_by_key(|(s, _, _)| *s);

    Some(annotate_source(code, &annotations))
}

/// Recursively collects byte-range colour annotations from tree-sitter nodes,
/// classifying each node kind via [`node_color`].
fn collect_annotations(
    l: TsLang,
    node: tree_sitter::Node<'_>,
    p: &Palette,
    out: &mut Vec<(usize, usize, Color)>,
) {
    if let Some(color) = node_color(l, node.kind(), p) {
        out.push((node.start_byte(), node.end_byte(), color));
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_annotations(l, child, p, out);
    }
}

/// Splits `source` into per-line [`StyledLine`]s using byte-range colour
/// annotations.  Unannotated bytes get the default code colour.
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

            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut pos = line_start;

            let relevant: Vec<_> = annotations
                .iter()
                .filter(|(s, e, _)| *e > line_start && *s < line_end)
                .collect();

            for &&(ann_start, ann_end, color) in &relevant {
                let seg_start = ann_start.max(line_start);
                let seg_end = ann_end.min(line_end);
                if seg_start > pos {
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
