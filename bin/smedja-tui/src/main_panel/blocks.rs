//! Diff / code-block line rendering for the transcript panel: unified-diff
//! marker detection and gutter rendering, inline-markdown and table span
//! builders, and LaTeX-to-Unicode math rendering. Moved verbatim from
//! `main_panel.rs`; `super` is the panel module.

use crate::theme::palette;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// True for unified-diff header/hunk lines whose syntax is specific enough to
/// recognise outside a fenced block without false-positiving on prose. Content
/// (`+`/`-`) lines are intentionally excluded — bare `+`/`-` outside a diff is
/// handled by the existing add/remove classification.
pub(crate) fn is_diff_marker(text: &str) -> bool {
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

/// True for a transcript line that is *chrome* — external-runner tool echo,
/// execute banner, or clipboard notice — and must recede to dim so the
/// assistant's answer body reads brighter than the scaffolding around it.
///
/// External CLI runners (codex, claude) print these flat, full-weight lines
/// around their answer; without this they render at the same weight as the
/// content. Native runners emit tool *cards* (already dim, via
/// `push_styled_line`), so this only ever fires on passthrough text lines.
///
/// - `↳ ok · …` / `↳ error · …` tool-result echo and the `↳ N↑ M↓ · trace: …`
///   turn footer.
/// - `⏵ execute · <cmd> · ✓ 0ms` execute banner.
/// - `✓ 63 lines copied to clipboard` (and `✗ …`) clipboard notices.
pub(crate) fn is_dim_chrome(text: &str) -> bool {
    let t = text.trim_start();
    if t.starts_with('\u{21b3}') {
        return true; // ↳ result echo / turn footer
    }
    if t.starts_with('\u{23f5}') {
        return true; // ⏵ execute banner
    }
    if (t.starts_with('\u{2713}') || t.starts_with('\u{2717}')) && t.contains("copied to clipboard")
    {
        return true; // ✓/✗ clipboard notice
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
pub(crate) fn inline_markdown_spans(text: &str) -> Option<Line<'static>> {
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
        // [label](url) link → label reads as an accent-underlined anchor and the
        // url recedes to dim, so a link gains hierarchy without shouting the
        // (often long) target.
        if c == '[' {
            if let Some(close_br) = (i + 1..n).find(|&j| chars[j] == ']') {
                let has_paren = close_br > i + 1 && close_br + 1 < n && chars[close_br + 1] == '(';
                let close_par = if has_paren {
                    (close_br + 2..n).find(|&j| chars[j] == ')')
                } else {
                    None
                };
                if let Some(close_par) = close_par {
                    if !buf.is_empty() {
                        spans.push(Span::raw(std::mem::take(&mut buf)));
                    }
                    let label: String = chars[i + 1..close_br].iter().collect();
                    let url: String = chars[close_br + 2..close_par].iter().collect();
                    spans.push(Span::styled(
                        label,
                        Style::default()
                            .fg(p.accent)
                            .add_modifier(Modifier::UNDERLINED),
                    ));
                    spans.push(Span::styled(
                        format!(" ({url})"),
                        Style::default().fg(p.text_dim).add_modifier(Modifier::DIM),
                    ));
                    i = close_par + 1;
                    found = true;
                    continue;
                }
            }
        }
        // `inline code`
        if c == '`' {
            if let Some(close) = (i + 1..n).find(|&j| chars[j] == '`') {
                if close > i + 1 {
                    if !buf.is_empty() {
                        spans.push(Span::raw(std::mem::take(&mut buf)));
                    }
                    let code: String = chars[i + 1..close].iter().collect();
                    // Subtle warm bg tint so inline code pops out of prose.
                    spans.push(Span::styled(
                        code,
                        Style::default().fg(p.text_bright).bg(p.header),
                    ));
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

/// Renders a block-level markdown construct — ATX heading, blockquote,
/// thematic break (horizontal rule), or list item — into styled spans, or
/// `None` when the line is ordinary prose (so the cheap plain/inline path runs).
///
/// The returned line's flattened text is the *display* text (heading `#`
/// markers and list bullets normalised), matching what is copied. Inline markup
/// inside a heading/quote/list is expanded via [`inline_markdown_spans`] so
/// `**bold**` still styles inside a `> quote`.
///
/// - `# … ######` heading → bold accent, marker stripped (deeper levels dimmer).
/// - `> quote` → dim italic body behind a coloured `▍` gutter bar.
/// - `---` / `***` / `___` rule → a full-width-ish dim `─` thematic break.
/// - `- ` / `* ` / `+ ` bullet → aligned `•`; `N.` ordered → accent number.
pub(crate) fn block_markdown_spans(text: &str) -> Option<Line<'static>> {
    let p = palette();
    let trimmed = text.trim_start();
    let indent = &text[..text.len() - trimmed.len()];

    // Thematic break: a line of only -, *, or _ (>= 3), optionally spaced.
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() >= 3
        && (compact.chars().all(|c| c == '-')
            || compact.chars().all(|c| c == '*')
            || compact.chars().all(|c| c == '_'))
    {
        let rule = "\u{2500}".repeat(24);
        return Some(Line::from(Span::styled(
            rule,
            Style::default()
                .fg(p.border_dim)
                .add_modifier(Modifier::DIM),
        )));
    }

    // ATX heading: 1–6 leading '#', then a space.
    let level = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&level) {
        let after = &trimmed[level..];
        if after.starts_with(' ') || after.is_empty() {
            let body = after.trim_start();
            // H1/H2 bright accent + bold; deeper headings step down to plain
            // accent so the outline reads as a hierarchy, not a shout.
            let color = if level <= 2 { p.text_bright } else { p.accent };
            return Some(Line::from(Span::styled(
                body.to_owned(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )));
        }
    }

    // Blockquote: '> ' possibly nested. Dim italic behind a coloured gutter.
    if let Some(rest) = trimmed.strip_prefix('>') {
        let body = rest.strip_prefix(' ').unwrap_or(rest);
        let mut spans = vec![Span::styled("\u{258d} ", Style::default().fg(p.accent))];
        let inner =
            inline_markdown_spans(body).unwrap_or_else(|| Line::from(Span::raw(body.to_owned())));
        for s in inner.spans {
            let style = s.style.fg.map_or_else(
                || {
                    Style::default()
                        .fg(p.text_dim)
                        .add_modifier(Modifier::ITALIC)
                },
                |_| s.style,
            );
            spans.push(Span::styled(s.content, style));
        }
        return Some(Line::from(spans));
    }

    // Unordered list item: '- ', '* ', or '+ ' after optional indent — plus the
    // already-rendered bullet glyphs ('•', '◦', '‣') an external CLI (codex,
    // claude) prints for its own lists, so its bullets gain the same aligned
    // treatment as raw-markdown ones instead of reading as flat prose.
    for marker in ['-', '*', '+', '\u{2022}', '\u{25e6}', '\u{2023}'] {
        let pre = format!("{marker} ");
        if let Some(rest) = trimmed.strip_prefix(&pre) {
            let mut spans = vec![
                Span::raw(indent.to_owned()),
                Span::styled("\u{2022} ", Style::default().fg(p.accent)),
            ];
            let inner = inline_markdown_spans(rest)
                .unwrap_or_else(|| Line::from(Span::raw(rest.to_owned())));
            spans.extend(inner.spans);
            return Some(Line::from(spans));
        }
    }

    // Ordered list item: 'N.' or 'N)' then a space.
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() && digits.len() <= 3 {
        let after = &trimmed[digits.len()..];
        if let Some(rest) = after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix(") "))
        {
            let mut spans = vec![
                Span::raw(indent.to_owned()),
                Span::styled(
                    format!("{digits}. "),
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
            ];
            let inner = inline_markdown_spans(rest)
                .unwrap_or_else(|| Line::from(Span::raw(rest.to_owned())));
            spans.extend(inner.spans);
            return Some(Line::from(spans));
        }
    }

    None
}

/// True for a markdown table row — a trimmed line that starts with `|` and has
/// at least one more `|`. Requiring a leading pipe avoids false-positiving on
/// prose that merely contains a pipe.
pub(crate) fn is_table_row(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('|') && t.get(1..).is_some_and(|rest| rest.contains('|'))
}

/// Splits a table row into trimmed cell texts (outer pipes stripped).
pub(crate) fn table_cells(text: &str) -> Vec<String> {
    let t = text.trim();
    let inner = t.strip_prefix('|').unwrap_or(t);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    inner.split('|').map(|c| c.trim().to_owned()).collect()
}

/// Renders a markdown table row: a `|---|` delimiter row becomes a horizontal
/// rule, any other row becomes its cells joined by a dim `│`. (Columns are not
/// auto-aligned across rows — a per-line pass that streams cleanly.)
pub(crate) fn table_row_spans(text: &str) -> Line<'static> {
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
pub(crate) fn diff_line_spans(text: &str) -> Line<'static> {
    let p = palette();
    // A file-path header is a bold dim label; the hunk locator and context are
    // dim so the eye lands on the +/- content, not the diff chrome.
    let header = Style::default().fg(p.text_dim).add_modifier(Modifier::BOLD);
    let hunk = Style::default().fg(p.text_dim).add_modifier(Modifier::DIM);

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
pub(crate) fn render_math(text: &str) -> String {
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
