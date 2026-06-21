//! Turn block rendering — framed blocks with state machine for streaming turns.

use ratatui::text::{Line, Text};
use ratatui::widgets::{Paragraph, Widget};

/// State of a turn block in the rendering pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // ToolCall variant is constructed by the future streaming RPC path
pub enum BlockStatus {
    Streaming,
    ToolCall { name: String },
    Complete,
    Failed,
}

/// A single tool call entry within a turn block.
#[derive(Debug, Clone)]
pub struct ToolEntry {
    pub name: String,
    pub args: String,
    pub outcome: Option<String>,
    #[allow(dead_code)] // read by inline_diff and the future diff-viewer key binding
    pub diff: Option<String>,
}

/// Tracks a single assistant turn from start to completion.
#[derive(Debug, Clone)]
pub struct TurnBlock {
    pub turn_n: u32,
    pub content: String,
    pub elapsed_ms: u64,
    pub status: BlockStatus,
    pub tool_calls: Vec<ToolEntry>,
}

impl TurnBlock {
    /// Creates a new streaming turn block.
    pub fn new(turn_n: u32) -> Self {
        Self {
            turn_n,
            content: String::new(),
            elapsed_ms: 0,
            status: BlockStatus::Streaming,
            tool_calls: Vec::new(),
        }
    }

    /// Appends streamed text.
    pub fn push_text(&mut self, text: &str) {
        self.content.push_str(text);
    }

    /// Records a tool call start.
    #[allow(dead_code)] // called by the future streaming RPC event handler
    pub fn push_tool_call(&mut self, name: String, args: String) {
        self.status = BlockStatus::ToolCall { name: name.clone() };
        self.tool_calls.push(ToolEntry {
            name,
            args,
            outcome: None,
            diff: None,
        });
    }

    /// Updates the last tool call with its outcome.
    #[allow(dead_code)] // called by the future streaming RPC event handler
    pub fn set_tool_outcome(&mut self, outcome: String) {
        if let Some(entry) = self.tool_calls.last_mut() {
            // Extract diff if outcome looks like a unified diff.
            if outcome.contains("@@") && (outcome.contains("\n+") || outcome.contains("\n-")) {
                entry.diff = Some(outcome.clone());
            }
            entry.outcome = Some(outcome);
        }
        self.status = BlockStatus::Streaming;
    }

    /// Marks the block complete.
    pub fn complete(&mut self, elapsed_ms: u64) {
        self.elapsed_ms = elapsed_ms;
        self.status = BlockStatus::Complete;
    }

    /// Marks the block failed.
    pub fn fail(&mut self) {
        self.status = BlockStatus::Failed;
    }

    /// Renders the block as display lines.
    pub fn render_lines(&self, width: usize) -> Vec<String> {
        let fill = width.saturating_sub(4);
        let bar = "─".repeat(fill);
        let status_str = match &self.status {
            BlockStatus::Streaming => "streaming\u{2026}",
            BlockStatus::ToolCall { name } => name.as_str(),
            BlockStatus::Complete => "complete",
            BlockStatus::Failed => "failed",
        };
        let header = format!("┌─ turn {} ─{}┐", self.turn_n, bar);
        let footer = format!("└─ {} · {}ms {}┘", status_str, self.elapsed_ms, bar);
        let mut lines = vec![header];
        for entry in &self.tool_calls {
            lines.push(format!("│ \u{25b8} {} {}", entry.name, entry.args));
            if let Some(outcome) = &entry.outcome {
                let short: String = outcome
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(60)
                    .collect();
                lines.push(format!("│   └ {short}"));
            }
        }
        for line in self.content.lines() {
            lines.push(format!("│ {line}"));
        }
        lines.push(footer);
        lines
    }

    /// Returns up to `max_lines` of the diff for the selected tool entry, or `None`.
    #[allow(dead_code)] // used by the future `d`-key inline diff viewer
    pub fn inline_diff(&self, entry_idx: usize, max_lines: usize) -> Option<Vec<String>> {
        let entry = self.tool_calls.get(entry_idx)?;
        let diff = entry.diff.as_ref()?;
        Some(diff.lines().take(max_lines).map(str::to_owned).collect())
    }
}

/// Ring buffer of completed turn blocks for the current session.
///
/// Capacity is fixed at 200; oldest entries are evicted when the limit is
/// reached.
#[allow(dead_code)] // session block history; wired to TUI history panel in upcoming story
#[derive(Debug, Default)]
pub struct BlockStore {
    blocks: std::collections::VecDeque<TurnBlock>,
}

#[allow(dead_code)] // session block history; wired to TUI history panel in upcoming story
impl BlockStore {
    /// Creates an empty [`BlockStore`] pre-allocated for 200 entries.
    #[must_use]
    pub fn new() -> Self {
        Self {
            blocks: std::collections::VecDeque::with_capacity(200),
        }
    }

    /// Appends a completed block; evicts the oldest entry if over capacity.
    pub fn push(&mut self, block: TurnBlock) {
        if self.blocks.len() >= 200 {
            self.blocks.pop_front();
        }
        self.blocks.push_back(block);
    }

    /// Returns an iterator over all stored blocks in chronological order.
    pub fn blocks(&self) -> impl Iterator<Item = &TurnBlock> + '_ {
        self.blocks.iter()
    }

    /// Returns the total count of stored blocks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Returns `true` when no blocks are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

impl Widget for TurnBlock {
    fn render(self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        let lines: Vec<Line<'_>> = self
            .render_lines(area.width as usize)
            .into_iter()
            .map(Line::raw)
            .collect();
        Paragraph::new(Text::from(lines)).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_block_is_streaming() {
        let b = TurnBlock::new(1);
        assert_eq!(b.status, BlockStatus::Streaming);
    }

    #[test]
    fn push_tool_call_changes_status() {
        let mut b = TurnBlock::new(1);
        b.push_tool_call("bash".into(), "ls".into());
        assert!(matches!(b.status, BlockStatus::ToolCall { .. }));
    }

    #[test]
    fn set_tool_outcome_returns_to_streaming() {
        let mut b = TurnBlock::new(1);
        b.push_tool_call("read".into(), "foo.rs".into());
        b.set_tool_outcome("file content".into());
        assert_eq!(b.status, BlockStatus::Streaming);
        assert_eq!(b.tool_calls[0].outcome.as_deref(), Some("file content"));
    }

    #[test]
    fn complete_sets_elapsed() {
        let mut b = TurnBlock::new(2);
        b.complete(150);
        assert_eq!(b.elapsed_ms, 150);
        assert_eq!(b.status, BlockStatus::Complete);
    }

    #[test]
    fn render_lines_has_header_and_footer() {
        let mut b = TurnBlock::new(1);
        b.push_text("hello");
        b.complete(42);
        let lines = b.render_lines(40);
        assert!(lines[0].starts_with("┌─ turn 1"));
        assert!(lines.last().unwrap().contains("42ms"));
    }

    #[test]
    fn render_lines_multiline_content() {
        let mut b = TurnBlock::new(2);
        b.push_text("a\nb");
        b.complete(0);
        // header + 2 content lines + footer = 4 lines
        let lines = b.render_lines(30);
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn diff_extracted_from_tool_outcome() {
        let mut b = TurnBlock::new(1);
        b.push_tool_call("edit_file".into(), "foo.rs".into());
        b.set_tool_outcome("@@ -1,3 +1,4 @@\n context\n+added line\n-removed".into());
        assert!(b.tool_calls[0].diff.is_some());
    }

    #[test]
    fn inline_diff_truncates_to_max_lines() {
        let mut b = TurnBlock::new(1);
        b.push_tool_call("edit_file".into(), "x".into());
        let long_diff = (0..30)
            .map(|i| format!("+line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        b.set_tool_outcome(format!("@@ -1 +1 @@\n{long_diff}"));
        let lines = b.inline_diff(0, 5).unwrap();
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn push_respects_capacity() {
        let mut store = BlockStore::new();
        for i in 0..=200u32 {
            store.push(TurnBlock::new(i));
        }
        assert_eq!(store.len(), 200);
    }

    #[test]
    fn push_evicts_oldest() {
        let mut store = BlockStore::new();
        for i in 0..=200u32 {
            store.push(TurnBlock::new(i));
        }
        let first = store.blocks().next().expect("store should not be empty");
        assert_eq!(first.turn_n, 1);
    }

    #[test]
    fn is_empty_on_new() {
        let store = BlockStore::new();
        assert!(store.is_empty());
    }

    #[test]
    fn inline_diff_preserves_prefixes() {
        let mut b = TurnBlock::new(1);
        b.push_tool_call("edit_file".into(), "a.rs".into());
        b.set_tool_outcome("@@ -1,2 +1,2 @@\n-old\n+new".into());
        let lines = b.inline_diff(0, 20).unwrap();
        assert!(lines.iter().any(|l| l.starts_with('-')));
        assert!(lines.iter().any(|l| l.starts_with('+')));
    }
}
