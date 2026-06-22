//! `st-agent` — agent block rendering, smdjad UDS client, and approval flow for smedja.
//!
//! Provides the bridge between the smedja daemon (smdjad) and the terminal
//! renderer: streaming agent responses arrive as events, are accumulated into
//! [`AgentSession`] state, and are surfaced to the renderer via
//! [`st_render::AgentBlockView`].
//!
//! Phase 5 additions: [`SmdjadClient`], [`PaneEvent`], [`PaneAgentState`],
//! [`SharedPaneState`], [`ApprovalGate`], socket discovery helpers, and
//! per-pane env-var injection.

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};
use uuid::Uuid;

// ─────────────────────────────────────────────────────────────────────────────
// Socket discovery
// ─────────────────────────────────────────────────────────────────────────────

/// Returns the path to the smdjad Unix domain socket.
///
/// Uses `$XDG_RUNTIME_DIR/smdjad.sock` when `XDG_RUNTIME_DIR` is set,
/// otherwise falls back to `/tmp/smdjad.sock`.
#[must_use]
pub fn smdjad_socket_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(xdg).join("smdjad.sock")
    } else {
        PathBuf::from("/tmp/smdjad.sock")
    }
}

/// Returns `true` if the smdjad socket exists on the filesystem.
pub async fn socket_exists() -> bool {
    tokio::fs::metadata(smdjad_socket_path()).await.is_ok()
}

// ─────────────────────────────────────────────────────────────────────────────
// Pane environment-variable injection
// ─────────────────────────────────────────────────────────────────────────────

/// Returns the `(key, value)` pair to inject into a child process environment
/// so that the agent inside the pane can report its pane identity to smdjad.
#[must_use]
pub fn pane_env_var(pane_id: &Uuid) -> (String, String) {
    ("SMEDJA_TERM_PANE".into(), pane_id.to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// Events from smdjad
// ─────────────────────────────────────────────────────────────────────────────

/// Events published by smdjad on a pane subscription.
#[derive(Debug, Clone)]
pub enum PaneEvent {
    /// An agent turn has started.
    TurnStart {
        session_id: String,
        turn_id: String,
        tier: String,
        model: String,
        /// W3C trace-id for distributed tracing correlation.
        trace_id: Option<String>,
        /// W3C span-id from the span that produced this event.
        span_id: Option<String>,
    },
    /// The agent is invoking a tool.
    ToolCall {
        tool_name: String,
        args_summary: String,
        /// Tool-call identifier for correlating the call with its result.
        tool_call_id: Option<String>,
        /// W3C trace-id for distributed tracing correlation.
        trace_id: Option<String>,
        /// W3C span-id from the span that produced this event.
        span_id: Option<String>,
    },
    /// The agent requires interactive approval before executing a tool.
    ApprovalPrompt {
        tool_name: String,
        args: Value,
        prompt: String,
    },
    /// A tool invocation has completed.
    ToolResult { tool_name: String, outcome: String },
    /// The agent turn has finished; token and latency counters are attached.
    TurnEnd {
        input_tokens: u64,
        output_tokens: u64,
        latency_ms: u64,
    },
    /// Incremental text from the model stream.
    StreamDelta { text: String },
}

impl PaneEvent {
    /// Deserialises a [`PaneEvent`] from a raw JSON line received from smdjad.
    ///
    /// Returns `None` when the line is not a recognised event type.
    pub fn from_json_line(line: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(line).ok()?;
        let event_type = v.get("type")?.as_str()?;
        match event_type {
            "turn_start" => {
                let p = v.get("params")?;
                Some(Self::TurnStart {
                    session_id: p.get("session_id")?.as_str()?.to_owned(),
                    turn_id: p.get("turn_id")?.as_str()?.to_owned(),
                    tier: p.get("tier")?.as_str()?.to_owned(),
                    model: p.get("model")?.as_str()?.to_owned(),
                    trace_id: p
                        .get("trace_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                    span_id: p.get("span_id").and_then(|v| v.as_str()).map(str::to_owned),
                })
            }
            "tool_call" => {
                let p = v.get("params")?;
                Some(Self::ToolCall {
                    tool_name: p.get("tool_name")?.as_str()?.to_owned(),
                    args_summary: p.get("args_summary")?.as_str()?.to_owned(),
                    tool_call_id: p
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                    trace_id: p
                        .get("trace_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                    span_id: p.get("span_id").and_then(|v| v.as_str()).map(str::to_owned),
                })
            }
            "approval_prompt" => {
                let p = v.get("params")?;
                Some(Self::ApprovalPrompt {
                    tool_name: p.get("tool_name")?.as_str()?.to_owned(),
                    args: p.get("args")?.clone(),
                    prompt: p.get("prompt")?.as_str()?.to_owned(),
                })
            }
            "tool_result" => {
                let p = v.get("params")?;
                Some(Self::ToolResult {
                    tool_name: p.get("tool_name")?.as_str()?.to_owned(),
                    outcome: p.get("outcome")?.as_str()?.to_owned(),
                })
            }
            "turn_end" => {
                let p = v.get("params")?;
                Some(Self::TurnEnd {
                    input_tokens: p.get("input_tokens")?.as_u64()?,
                    output_tokens: p.get("output_tokens")?.as_u64()?,
                    latency_ms: p.get("latency_ms")?.as_u64()?,
                })
            }
            "stream_delta" => {
                let p = v.get("params")?;
                Some(Self::StreamDelta {
                    text: p.get("text")?.as_str()?.to_owned(),
                })
            }
            other => {
                warn!(event_type = other, "unrecognised smdjad event");
                None
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Approval decision
// ─────────────────────────────────────────────────────────────────────────────

/// The user's decision on an [`ApprovalGate`] prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// The user approved the pending tool call.
    Approve,
    /// The user denied the pending tool call.
    Deny,
}

// ─────────────────────────────────────────────────────────────────────────────
// SmdjadClient
// ─────────────────────────────────────────────────────────────────────────────

/// Async tokio UDS client connected to the smdjad daemon.
///
/// The protocol is newline-delimited JSON. After connecting the caller should
/// call [`subscribe_pane`](SmdjadClient::subscribe_pane) to start receiving
/// [`PaneEvent`]s via [`next_event`](SmdjadClient::next_event).
pub struct SmdjadClient {
    reader: BufReader<tokio::io::ReadHalf<UnixStream>>,
    writer: tokio::io::WriteHalf<UnixStream>,
}

impl SmdjadClient {
    /// Opens a connection to the smdjad socket at the path returned by
    /// [`smdjad_socket_path`].
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the socket does not exist or the connection
    /// is refused.
    pub async fn connect() -> Result<Self, io::Error> {
        let stream = UnixStream::connect(smdjad_socket_path()).await?;
        let (read_half, writer) = tokio::io::split(stream);
        let reader = BufReader::new(read_half);
        debug!("connected to smdjad socket");
        Ok(Self { reader, writer })
    }

    /// Sends a `subscribe_pane` request for the given pane UUID.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if serialisation fails or the write to the
    /// socket fails.
    pub async fn subscribe_pane(&mut self, pane_id: &str) -> Result<(), io::Error> {
        let msg = serde_json::json!({
            "method": "subscribe_pane",
            "params": { "pane_id": pane_id }
        });
        let mut line = serde_json::to_string(&msg)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        debug!(pane_id, "subscribed to pane");
        Ok(())
    }

    /// Reads the next event from the smdjad stream.
    ///
    /// Returns `Ok(None)` on EOF.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if reading from the socket fails.
    pub async fn next_event(&mut self) -> Result<Option<PaneEvent>, io::Error> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            return Ok(None);
        }
        debug!(line = trimmed, "received smdjad line");
        Ok(PaneEvent::from_json_line(trimmed))
    }

    /// Sends an approval decision for a pending tool call.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if serialisation fails or the write to the
    /// socket fails.
    pub async fn send_approval(
        &mut self,
        pane_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), io::Error> {
        let approved = decision == ApprovalDecision::Approve;
        let msg = serde_json::json!({
            "method": "approval_response",
            "params": {
                "pane_id": pane_id,
                "approved": approved,
            }
        });
        let mut line = serde_json::to_string(&msg)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        info!(pane_id, approved, "sent approval response");
        Ok(())
    }
}

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
    /// [`SmdjadClient`] connection is active.
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

    // ── Phase 2 (retained) ────────────────────────────────────────────────

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

    // ── Phase 5 ───────────────────────────────────────────────────────────

    #[test]
    fn smdjad_socket_path_uses_xdg_runtime_dir() {
        // Temporarily set XDG_RUNTIME_DIR; restore afterward.
        let _guard = EnvGuard::set("XDG_RUNTIME_DIR", "/run/user/1000");
        let path = smdjad_socket_path();
        assert_eq!(path, PathBuf::from("/run/user/1000/smdjad.sock"));
    }

    #[test]
    fn socket_path_matches_smdjad() {
        // st-agent and smdjad must agree on the socket path for a given XDG_RUNTIME_DIR.
        // This test verifies the st-agent path matches the expected format.
        let _guard = EnvGuard::set("XDG_RUNTIME_DIR", "/run/user/9999");
        let path = smdjad_socket_path();
        assert_eq!(
            path.to_str().unwrap(),
            "/run/user/9999/smdjad.sock",
            "socket path must be $XDG_RUNTIME_DIR/smdjad.sock"
        );
        // Confirm no subdirectory: path should not contain /smedja/
        assert!(
            !path.to_str().unwrap().contains("/smedja/"),
            "socket path must not contain /smedja/ subdirectory"
        );
    }

    #[test]
    fn smdjad_socket_path_falls_back_to_tmp() {
        let _guard = EnvGuard::remove("XDG_RUNTIME_DIR");
        let path = smdjad_socket_path();
        assert_eq!(path, PathBuf::from("/tmp/smdjad.sock"));
    }

    #[test]
    fn pane_event_deserialise_turn_start() {
        let line = r#"{"type":"turn_start","params":{"session_id":"s1","turn_id":"t1","tier":"pro","model":"claude-opus-4"}}"#;
        let event = PaneEvent::from_json_line(line).expect("should parse");
        if let PaneEvent::TurnStart {
            session_id,
            turn_id,
            tier,
            model,
            ..
        } = event
        {
            assert_eq!(session_id, "s1");
            assert_eq!(turn_id, "t1");
            assert_eq!(tier, "pro");
            assert_eq!(model, "claude-opus-4");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn pane_event_deserialise_tool_call() {
        let line = r#"{"type":"tool_call","params":{"tool_name":"bash","args_summary":"ls -la"}}"#;
        let event = PaneEvent::from_json_line(line).expect("should parse");
        if let PaneEvent::ToolCall {
            tool_name,
            args_summary,
            ..
        } = event
        {
            assert_eq!(tool_name, "bash");
            assert_eq!(args_summary, "ls -la");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn pane_event_deserialise_approval_prompt() {
        let line = r#"{"type":"approval_prompt","params":{"tool_name":"rm","args":{"path":"/tmp/x"},"prompt":"Allow deletion?"}}"#;
        let event = PaneEvent::from_json_line(line).expect("should parse");
        if let PaneEvent::ApprovalPrompt {
            tool_name,
            args,
            prompt,
        } = event
        {
            assert_eq!(tool_name, "rm");
            assert_eq!(args["path"], "/tmp/x");
            assert_eq!(prompt, "Allow deletion?");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn pane_event_deserialise_turn_end() {
        let line = r#"{"type":"turn_end","params":{"input_tokens":100,"output_tokens":200,"latency_ms":350}}"#;
        let event = PaneEvent::from_json_line(line).expect("should parse");
        if let PaneEvent::TurnEnd {
            input_tokens,
            output_tokens,
            latency_ms,
        } = event
        {
            assert_eq!(input_tokens, 100);
            assert_eq!(output_tokens, 200);
            assert_eq!(latency_ms, 350);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn approval_gate_render_lines_contains_tool_name() {
        let gate = ApprovalGate {
            pane_id: "pane-1".into(),
            tool_name: "bash".into(),
            args: serde_json::json!({"cmd": "ls"}),
            prompt: "Allow bash?".into(),
            state: ApprovalState::Pending,
        };
        let lines = gate.render_lines();
        assert!(
            lines.iter().any(|l| l.contains("bash")),
            "render_lines must mention the tool name"
        );
    }

    #[test]
    fn pane_env_var_returns_correct_key() {
        let id = Uuid::new_v4();
        let (key, val) = pane_env_var(&id);
        assert_eq!(key, "SMEDJA_TERM_PANE");
        assert_eq!(val, id.to_string());
    }

    #[test]
    fn shared_pane_state_is_clone() {
        let s = SharedPaneState::new();
        let _s2 = s.clone();
    }

    #[test]
    fn agent_session_suppress_flag_defaults_false() {
        let s = AgentSession::new("b", "m");
        assert!(!s.suppress_pty_output);
    }

    // ── Phase 6 ───────────────────────────────────────────────────────────

    /// Verifies that `smedja_bellows::event::TurnEvent` can decode JSON that
    /// includes the new optional correlation fields (`conversation_id`,
    /// `trace_id`, etc.) without error, and that the decoded variant is
    /// `TurnEvent::Started`.  The correlation fields are available for
    /// forwarding to the renderer via the event stream.
    #[test]
    fn st_agent_decodes_enriched_started_event() {
        let json = r#"{"Started":{"session_id":"s","turn_id":"t","conversation_id":"c","trace_id":"tid"}}"#;
        let ev: smedja_bellows::event::TurnEvent = serde_json::from_str(json).unwrap();
        assert!(
            matches!(ev, smedja_bellows::event::TurnEvent::Started { .. }),
            "expected TurnEvent::Started"
        );
    }

    /// Verifies backward compatibility: a JSON payload that has no correlation
    /// fields (e.g. from an older daemon) still deserializes successfully and
    /// all new optional fields default to `None`.
    #[test]
    fn st_agent_decodes_legacy_started_event() {
        let json = r#"{"Started":{"session_id":"old","turn_id":"t0"}}"#;
        let ev: smedja_bellows::event::TurnEvent = serde_json::from_str(json).unwrap();
        if let smedja_bellows::event::TurnEvent::Started {
            conversation_id,
            trace_id,
            agent_name,
            ..
        } = ev
        {
            assert!(conversation_id.is_none());
            assert!(trace_id.is_none());
            assert!(agent_name.is_none());
        } else {
            panic!("expected TurnEvent::Started");
        }
    }

    // ── Test helpers ─────────────────────────────────────────────────────

    /// RAII guard that sets or removes an environment variable and restores the
    /// original value on drop.  Using this avoids cross-test pollution when
    /// tests run in the same process.
    struct EnvGuard {
        key: String,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            // SAFETY: single-threaded test context; we restore on drop.
            unsafe { std::env::set_var(key, value) };
            Self {
                key: key.to_owned(),
                previous,
            }
        }

        fn remove(key: &str) -> Self {
            let previous = std::env::var(key).ok();
            // SAFETY: single-threaded test context; we restore on drop.
            unsafe { std::env::remove_var(key) };
            Self {
                key: key.to_owned(),
                previous,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => unsafe { std::env::set_var(&self.key, v) },
                None => unsafe { std::env::remove_var(&self.key) },
            }
        }
    }
}
