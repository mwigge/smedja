//! Format-string substitution for rendered segments.

use crate::Segment;

/// Substitutes `$module_name` tokens in `format` with the matching segment text.
///
/// Unresolved tokens (modules not present in `segments`) are removed. Pipe
/// characters `|` are replaced with the box-drawing vertical `│` (U+2502).
#[must_use]
pub fn format_bar(segments: &[Segment], format: &str) -> String {
    let mut result = format.to_owned();

    // Replace matched tokens.
    for seg in segments {
        let token = format!("${}", seg.name);
        result = result.replace(&token, &seg.text);
    }

    // Remove leftover $tokens.
    let mut out = String::with_capacity(result.len());
    let mut chars = result.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            while chars
                .peek()
                .is_some_and(|ch| ch.is_alphanumeric() || *ch == '_')
            {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }

    // Replace ASCII pipe with box-drawing vertical bar.
    out.replace('|', "\u{2502}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SegmentStyle;

    // 7
    #[test]
    fn format_bar_replaces_module_tokens() {
        let segs = vec![Segment {
            name: "tier".to_owned(),
            text: "[local]".to_owned(),
            style: SegmentStyle::default(),
        }];
        let result = format_bar(&segs, "$tier active");
        assert_eq!(result, "[local] active");
    }

    // 8
    #[test]
    fn format_bar_separator_becomes_dim_char() {
        let result = format_bar(&[], "a | b");
        assert!(
            result.contains('\u{2502}'),
            "expected box-drawing │, got '{result}'"
        );
    }
}
