//! Size-capping and fenced-block assembly for resolved fragments.

use crate::fragments::resolve::Resolved;
use crate::fragments::Caps;

/// Appends a resolved fragment to `out`. Content blocks are size-capped (per
/// fragment and against the remaining message `budget`) and wrapped in a fenced
/// block tagged with `lang` and an optional `arg`; markers are injected verbatim.
pub(crate) fn push_block(
    out: &mut String,
    lang: &str,
    arg: &str,
    resolved: Resolved,
    caps: Caps,
    budget: &mut usize,
) {
    let content = match resolved {
        Resolved::Marker(marker) => {
            out.push_str(&marker);
            return;
        }
        Resolved::Content(c) => c,
    };

    let per_fragment_cap = caps.per_fragment_bytes.min(*budget);
    let capped = cap_content(&content, per_fragment_cap, caps.per_fragment_lines);
    *budget = budget.saturating_sub(capped.len());

    let header = if arg.is_empty() {
        format!("```{lang}\n")
    } else {
        format!("```{lang} {arg}\n")
    };
    out.push_str(&header);
    out.push_str(&capped);
    if !capped.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```");
}

/// Truncates `content` to at most `max_bytes` bytes and `max_lines` lines,
/// appending a `[smedja: truncated N bytes]` marker when anything is dropped.
/// Byte truncation respects UTF-8 char boundaries.
fn cap_content(content: &str, max_bytes: usize, max_lines: usize) -> String {
    // Line cap first: keep at most `max_lines` lines.
    let mut kept = content;
    let mut line_truncated = false;
    if content.lines().count() > max_lines {
        let mut end = 0usize;
        for (n, line) in content.split_inclusive('\n').enumerate() {
            if n >= max_lines {
                break;
            }
            end += line.len();
        }
        kept = &content[..end];
        line_truncated = true;
    }

    // Byte cap on the (possibly line-capped) slice, respecting char boundaries.
    let mut byte_truncated = false;
    let mut byte_end = kept.len();
    if kept.len() > max_bytes {
        byte_end = max_bytes;
        while byte_end > 0 && !kept.is_char_boundary(byte_end) {
            byte_end -= 1;
        }
        byte_truncated = true;
    }
    let final_slice = &kept[..byte_end];

    if !line_truncated && !byte_truncated {
        return content.to_owned();
    }
    let dropped = content.len() - final_slice.len();
    format!("{final_slice}\n[smedja: truncated {dropped} bytes]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_content_truncated_at_cap() {
        let content = "x".repeat(100);
        let capped = cap_content(&content, 10, 2_000);
        assert!(capped.starts_with(&"x".repeat(10)));
        assert!(
            capped.contains("[smedja: truncated 90 bytes]"),
            "marker: {capped}"
        );

        // Line cap.
        let many = "a\n".repeat(50);
        let capped = cap_content(&many, 1_000_000, 5);
        assert_eq!(
            capped.matches('\n').count() - 1,
            5,
            "5 lines kept: {capped}"
        );
        assert!(
            capped.contains("[smedja: truncated"),
            "line marker: {capped}"
        );
    }
}
