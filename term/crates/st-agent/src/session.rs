//! Agent session rendering state and the multi-session manager (Phase 2 types).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tracing::{debug, info};

// ─────────────────────────────────────────────────────────────────────────────
// Phase 2 types (retained from the original implementation)
// ─────────────────────────────────────────────────────────────────────────────

/// A single streaming chunk from an agent.
#[derive(Debug, Clone)]
pub struct AgentChunk {
    /// Block identifier (matches a `Block.id` in `st-blocks`).
    pub block_id: String,
    /// Incremental text delta.
    pub text: String,
    /// True when this is the last chunk for the block.
    pub done: bool,
    /// Non-zero if the agent is requesting approval for a tool call.
    pub approval_required: bool,
}

/// The approval state for an agent action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalState {
    /// No approval is pending.
    None,
    /// Waiting for the user to approve or deny.
    Pending,
    /// The user approved.
    Approved,
    /// The user denied.
    Denied,
}

/// An active agent session rendering state.
#[derive(Debug)]
pub struct AgentSession {
    /// Block identifier.
    pub block_id: String,
    /// Model name.
    pub model: String,
    /// Accumulated content lines.
    pub lines: VecDeque<String>,
    /// Current approval state.
    pub approval: ApprovalState,
    /// True while the agent is still streaming.
    pub streaming: bool,
    /// Maximum lines to keep in memory (oldest are discarded).
    pub max_lines: usize,
    /// When true the PTY event loop should suppress raw cell output because
    /// the renderer is handling agent output directly from smdjad events.
    ///
    /// Set to `true` while [`PaneAgentState::is_agent_turn`](crate::PaneAgentState::is_agent_turn)
    /// is true and a [`SmdjadClient`](crate::SmdjadClient) connection is active.
    pub suppress_pty_output: bool,
}

impl AgentSession {
    /// Creates a new [`AgentSession`] for `block_id` and `model`.
    #[must_use]
    pub fn new(block_id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            block_id: block_id.into(),
            model: model.into(),
            lines: VecDeque::new(),
            approval: ApprovalState::None,
            streaming: true,
            max_lines: 1000,
            suppress_pty_output: false,
        }
    }

    /// Appends a chunk of text to the session.
    ///
    /// The text is split on newlines and each line is stored separately.
    pub fn push_chunk(&mut self, chunk: &AgentChunk) {
        debug!(block_id = %chunk.block_id, "agent chunk received");
        for line in chunk.text.split('\n') {
            if self.lines.len() >= self.max_lines {
                self.lines.pop_front();
            }
            self.lines.push_back(line.to_owned());
        }
        if chunk.approval_required {
            self.approval = ApprovalState::Pending;
        }
        if chunk.done {
            self.streaming = false;
            info!(block_id = %chunk.block_id, "agent block complete");
        }
    }

    /// Approves the pending tool call.
    pub fn approve(&mut self) {
        self.approval = ApprovalState::Approved;
    }

    /// Denies the pending tool call.
    pub fn deny(&mut self) {
        self.approval = ApprovalState::Denied;
    }

    /// Returns the collected lines as a `Vec<String>`.
    #[must_use]
    pub fn content_lines(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AgentManager / SharedAgentManager (retained from Phase 2)
// ─────────────────────────────────────────────────────────────────────────────

/// Multi-session manager.
///
/// Keeps a map of active [`AgentSession`]s indexed by block ID.
#[derive(Debug, Default)]
pub struct AgentManager {
    sessions: std::collections::HashMap<String, AgentSession>,
}

impl AgentManager {
    /// Creates a new [`AgentManager`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a mutable reference to the session for `block_id`, creating it
    /// if it does not exist.
    pub fn session_mut(&mut self, block_id: &str, model: &str) -> &mut AgentSession {
        self.sessions
            .entry(block_id.to_owned())
            .or_insert_with(|| AgentSession::new(block_id, model))
    }

    /// Returns a reference to the session for `block_id`, if it exists.
    #[must_use]
    pub fn session(&self, block_id: &str) -> Option<&AgentSession> {
        self.sessions.get(block_id)
    }

    /// Removes and returns the session for `block_id`.
    pub fn remove(&mut self, block_id: &str) -> Option<AgentSession> {
        self.sessions.remove(block_id)
    }

    /// Returns the number of active sessions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Returns true if there are no active sessions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Returns an iterator over all active sessions.
    pub fn sessions(&self) -> impl Iterator<Item = &AgentSession> {
        self.sessions.values()
    }
}

/// Thread-safe wrapper around [`AgentManager`].
///
/// This is the type used in the main event loop where the renderer thread and
/// the RPC handler thread both need access.
#[derive(Clone, Default)]
pub struct SharedAgentManager(pub Arc<Mutex<AgentManager>>);

impl SharedAgentManager {
    /// Creates a new [`SharedAgentManager`].
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(AgentManager::new())))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_session_accumulates_lines() {
        let mut s = AgentSession::new("block1", "claude-opus");
        s.push_chunk(&AgentChunk {
            block_id: "block1".into(),
            text: "hello\nworld".into(),
            done: false,
            approval_required: false,
        });
        assert_eq!(s.content_lines(), vec!["hello", "world"]);
    }

    #[test]
    fn agent_session_done_stops_streaming() {
        let mut s = AgentSession::new("block1", "claude-opus");
        s.push_chunk(&AgentChunk {
            block_id: "block1".into(),
            text: "done".into(),
            done: true,
            approval_required: false,
        });
        assert!(!s.streaming);
    }

    #[test]
    fn agent_session_approval_pending_on_tool_call() {
        let mut s = AgentSession::new("block1", "claude-opus");
        s.push_chunk(&AgentChunk {
            block_id: "block1".into(),
            text: String::new(),
            done: false,
            approval_required: true,
        });
        assert_eq!(s.approval, ApprovalState::Pending);
    }

    #[test]
    fn agent_session_approve_changes_state() {
        let mut s = AgentSession::new("b", "m");
        s.approval = ApprovalState::Pending;
        s.approve();
        assert_eq!(s.approval, ApprovalState::Approved);
    }

    #[test]
    fn agent_session_deny_changes_state() {
        let mut s = AgentSession::new("b", "m");
        s.approval = ApprovalState::Pending;
        s.deny();
        assert_eq!(s.approval, ApprovalState::Denied);
    }

    #[test]
    fn agent_session_respects_max_lines() {
        let mut s = AgentSession::new("b", "m");
        s.max_lines = 3;
        for i in 0..5 {
            s.push_chunk(&AgentChunk {
                block_id: "b".into(),
                text: format!("line{i}"),
                done: false,
                approval_required: false,
            });
        }
        assert!(s.lines.len() <= 3);
    }

    #[test]
    fn agent_manager_creates_and_returns_session() {
        let mut mgr = AgentManager::new();
        let s = mgr.session_mut("b1", "model");
        s.push_chunk(&AgentChunk {
            block_id: "b1".into(),
            text: "hi".into(),
            done: false,
            approval_required: false,
        });
        assert_eq!(mgr.len(), 1);
        assert!(!mgr.is_empty());
    }

    #[test]
    fn agent_manager_remove_returns_session() {
        let mut mgr = AgentManager::new();
        mgr.session_mut("b1", "m");
        let s = mgr.remove("b1");
        assert!(s.is_some());
        assert!(mgr.is_empty());
    }

    #[test]
    fn shared_manager_is_clone() {
        let m = SharedAgentManager::new();
        let _m2 = m.clone();
    }

    #[test]
    fn agent_session_suppress_flag_defaults_false() {
        let s = AgentSession::new("b", "m");
        assert!(!s.suppress_pty_output);
    }
}
