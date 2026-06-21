//! Input staging queue for batched tool dispatch.

/// A staged tool action waiting to be dispatched.
#[derive(Debug, Clone)]
pub struct StagedAction {
    pub tool: String,
    pub args: serde_json::Value,
}

/// A staging queue for batched tool actions.
#[derive(Debug, Default)]
pub struct StagingQueue {
    pub items: Vec<StagedAction>,
}

impl StagingQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse and stage `/stage <tool> <json-args>`.
    ///
    /// Returns the staged action display string or an error message.
    ///
    /// # Errors
    ///
    /// Returns an error string when `args_json` is not valid JSON.
    pub fn stage(&mut self, tool: &str, args_json: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(args_json).map_err(|e| format!("invalid JSON args: {e}"))?;
        self.items.push(StagedAction {
            tool: tool.to_owned(),
            args,
        });
        Ok(format!("\u{23f8} staged: {tool}"))
    }

    /// Remove item at index N (1-based), or all items if N is `None`.
    pub fn unstage(&mut self, n: Option<usize>) -> String {
        match n {
            None => {
                let count = self.items.len();
                self.items.clear();
                format!("unstaged all ({count} items)")
            }
            Some(i) => {
                if i == 0 || i > self.items.len() {
                    format!("no item {i}")
                } else {
                    let removed = self.items.remove(i - 1);
                    format!("unstaged: {}", removed.tool)
                }
            }
        }
    }

    /// List remaining items.
    #[must_use]
    pub fn list(&self) -> Vec<String> {
        self.items
            .iter()
            .enumerate()
            .map(|(i, a)| format!("  {} \u{23f8} pending: {} {}", i + 1, a.tool, a.args))
            .collect()
    }

    /// Drain items for dispatch.
    pub fn drain(&mut self) -> Vec<StagedAction> {
        std::mem::take(&mut self.items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_adds_item() {
        let mut q = StagingQueue::new();
        let msg = q.stage("bash", r#"{"cmd":"ls"}"#).unwrap();
        assert_eq!(q.items.len(), 1);
        assert!(msg.contains("bash"));
    }

    #[test]
    fn stage_invalid_json_returns_err() {
        let mut q = StagingQueue::new();
        assert!(q.stage("bash", "not-json").is_err());
    }

    #[test]
    fn unstage_by_index_removes_correct_item() {
        let mut q = StagingQueue::new();
        q.stage("tool_a", "{}").unwrap();
        q.stage("tool_b", "{}").unwrap();
        q.stage("tool_c", "{}").unwrap();
        let msg = q.unstage(Some(2));
        assert!(msg.contains("tool_b"));
        assert_eq!(q.items.len(), 2);
        assert_eq!(q.items[0].tool, "tool_a");
        assert_eq!(q.items[1].tool, "tool_c");
    }

    #[test]
    fn unstage_all_clears_queue() {
        let mut q = StagingQueue::new();
        q.stage("a", "{}").unwrap();
        q.stage("b", "{}").unwrap();
        q.unstage(None);
        assert!(q.items.is_empty());
    }

    #[test]
    fn empty_drain_is_noop() {
        let mut q = StagingQueue::new();
        let drained = q.drain();
        assert!(drained.is_empty());
    }

    #[test]
    fn drain_returns_items_and_clears() {
        let mut q = StagingQueue::new();
        q.stage("x", "{}").unwrap();
        let drained = q.drain();
        assert_eq!(drained.len(), 1);
        assert!(q.items.is_empty());
    }
}
