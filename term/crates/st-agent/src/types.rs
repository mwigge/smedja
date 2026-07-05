//! Per-pane and per-session agent state types consumed by the renderer:
//! [`PaneAgentState`]/[`SharedPaneState`], the inline [`ApprovalGate`], and the
//! Phase 2 [`AgentSession`]/[`AgentManager`] accumulation types.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tracing::{debug, info};

use crate::events::PaneEvent;

// ─────────────────────────────────────────────────────────────────────────────
// PaneAgentState
// ─────────────────────────────────────────────────────────────────────────────

/// Per-pane live agent state consumed by the status bar renderer.
#[derive(Debug, Clone, Default)]
pub struct PaneAgentState {
    /// Tier string from the most recent `TurnStart` event (e.g. `"pro"`).
    pub tier: Option<String>,
    /// Model identifier from the most recent `TurnStart` event.
    pub model: Option<String>,
    /// Short description of what the agent is currently doing.
    pub active_task: Option<String>,
    /// True while an agent turn is in progress.
    pub is_agent_turn: bool,
    /// Input token count from the most recent `TurnEnd` event.
    pub last_input_tokens: Option<u64>,
    /// Output token count from the most recent `TurnEnd` event.
    pub last_output_tokens: Option<u64>,
    /// Turn latency in milliseconds from the most recent `TurnEnd` event.
    pub last_latency_ms: Option<u64>,
    /// W3C `traceparent` from the most recent `TurnEnd` event.
    pub last_traceparent: Option<String>,
    /// Cumulative tokens saved by the token economy, from the most recent
    /// `TurnEnd` that reported it. `None` until a figure arrives, so the
    /// status-bar segment renders nothing rather than a misleading zero.
    pub tokens_saved: Option<u64>,
    /// Cumulative efficiency ratio, from the most recent `TurnEnd` that
    /// reported it.
    pub efficiency_ratio: Option<f64>,
}

impl PaneAgentState {
    /// Applies a [`PaneEvent::TurnEnd`] to this state.
    ///
    /// Updates the per-turn token/latency counters and accumulates the
    /// cumulative token-economy figures. A `TurnEnd` that reports no savings
    /// figure leaves the previously accumulated value untouched, so a turn with
    /// no cache/compression activity never resets the gauge to a misleading
    /// zero. A non-`TurnEnd` event is ignored.
    pub fn apply_turn_end(&mut self, event: &PaneEvent) {
        let PaneEvent::TurnEnd {
            input_tokens,
            output_tokens,
            latency_ms,
            traceparent,
            tokens_saved,
            efficiency_ratio,
        } = event
        else {
            return;
        };
        self.is_agent_turn = false;
        self.last_input_tokens = Some(*input_tokens);
        self.last_output_tokens = Some(*output_tokens);
        self.last_latency_ms = Some(*latency_ms);
        self.last_traceparent.clone_from(traceparent);
        if let Some(saved) = *tokens_saved {
            self.tokens_saved = Some(saved);
        }
        if let Some(ratio) = *efficiency_ratio {
            self.efficiency_ratio = Some(ratio);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SharedPaneState
// ─────────────────────────────────────────────────────────────────────────────

/// Thread-safe, cheaply-cloneable wrapper around [`PaneAgentState`].
///
/// The status bar modules hold a clone of this and read it on every render
/// cycle; the event-loop task holds the same `Arc` and writes to it as events
/// arrive.
#[derive(Clone, Default)]
pub struct SharedPaneState(pub Arc<tokio::sync::RwLock<PaneAgentState>>);

impl SharedPaneState {
    /// Creates a new [`SharedPaneState`] backed by a default [`PaneAgentState`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ApprovalGate
// ─────────────────────────────────────────────────────────────────────────────

/// Inline approval-gate state for a single pending tool call.
///
/// Rendered by the terminal when an agent requests interactive approval.
pub struct ApprovalGate {
    /// Pane that owns this gate.
    pub pane_id: String,
    /// Name of the tool requesting approval.
    pub tool_name: String,
    /// Full argument object for the tool call.
    pub args: Value,
    /// Human-readable prompt from smdjad.
    pub prompt: String,
    /// Current approval state.
    pub state: ApprovalState,
}

impl ApprovalGate {
    /// Renders the approval gate as a list of display lines suitable for
    /// writing to the terminal.
    #[must_use]
    pub fn render_lines(&self) -> Vec<String> {
        let state_label = match self.state {
            ApprovalState::None | ApprovalState::Pending => "Pending",
            ApprovalState::Approved => "Approved",
            ApprovalState::Denied => "Denied",
        };
        let args_pretty =
            serde_json::to_string_pretty(&self.args).unwrap_or_else(|_| self.args.to_string());
        vec![
            format!("┌─ Approval required ─────────────────────────────"),
            format!("│  Tool   : {}", self.tool_name),
            format!("│  Prompt : {}", self.prompt),
            format!("│  Args   : {args_pretty}"),
            format!("│  State  : {state_label}"),
            format!("└─────────────────────────────────────────────────"),
        ]
    }
}

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
    /// Set to `true` while [`PaneAgentState::is_agent_turn`] is true and a
    /// [`SmdjadClient`](crate::SmdjadClient) connection is active.
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
