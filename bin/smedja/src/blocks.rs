//! Turn block rendering — wraps completed assistant turns in a visual frame.

/// A completed assistant turn, ready to render as a framed block.
#[derive(Debug, Clone)]
pub struct TurnBlock {
    pub turn_n: u32,
    pub content: String,
    pub elapsed_ms: u64,
}

impl TurnBlock {
    /// Renders the block as a `Vec` of display strings (header, content lines, footer).
    pub fn render_lines(&self, width: usize) -> Vec<String> {
        let bar = "─".repeat(width.saturating_sub(4));
        let header = format!("┌─ turn {} ─{}┐", self.turn_n, bar);
        let footer = format!("└─ complete · {}ms {}┘", self.elapsed_ms, bar);
        let mut lines = vec![header];
        for line in self.content.lines() {
            lines.push(format!("│ {line}"));
        }
        lines.push(footer);
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_lines_has_header_and_footer() {
        let block = TurnBlock {
            turn_n: 1,
            content: "hello".to_owned(),
            elapsed_ms: 42,
        };
        let lines = block.render_lines(40);
        assert!(lines[0].starts_with("┌─ turn 1"));
        assert!(lines.last().unwrap().contains("42ms"));
        assert!(lines[1].starts_with("│ hello"));
    }

    #[test]
    fn render_lines_multiline_content() {
        let block = TurnBlock {
            turn_n: 2,
            content: "a\nb".to_owned(),
            elapsed_ms: 0,
        };
        let lines = block.render_lines(30);
        // header + 2 content lines + footer = 4 lines
        assert_eq!(lines.len(), 4);
    }
}
