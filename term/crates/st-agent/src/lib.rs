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
//!
//! The implementation is split by concern across sibling modules and
//! re-exported here so that every `st_agent::X` path is unchanged:
//!
//! * [`events`] — the smdjad push-socket wire contract mapped onto [`PaneEvent`],
//!   plus the user's [`ApprovalDecision`].
//! * [`client`] — the [`SmdjadClient`] UDS client, socket discovery helpers, and
//!   per-pane env-var injection.
//! * [`types`] — per-pane / per-session state consumed by the renderer.

mod client;
mod events;
mod types;

pub use client::{
    agent_socket_path, pane_env_var, smdjad_socket_path, socket_exists, SmdjadClient,
};
pub use events::{ApprovalDecision, PaneEvent};
pub use types::{
    AgentChunk, AgentManager, AgentSession, ApprovalGate, ApprovalState, PaneAgentState,
    SharedAgentManager, SharedPaneState,
};

#[cfg(test)]
mod tests;
