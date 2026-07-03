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

mod approval;
mod client;
mod discovery;
mod event;
mod pane_state;
mod session;

pub use approval::ApprovalGate;
pub use client::SmdjadClient;
pub use discovery::{agent_socket_path, pane_env_var, smdjad_socket_path, socket_exists};
pub use event::{ApprovalDecision, PaneEvent};
pub use pane_state::{PaneAgentState, SharedPaneState};
pub use session::{AgentChunk, AgentManager, AgentSession, ApprovalState, SharedAgentManager};
