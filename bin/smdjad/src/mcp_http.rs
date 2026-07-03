//! MCP HTTP transport — JSON-RPC 2.0 client for tool discovery.
//!
//! Connects to an MCP server at a known URL. OAuth PKCE flow is a
//! future extension; for now, a static Bearer token is supported.

mod client;
mod notifications;
mod resources;
mod types;

pub use client::McpHttpClient;
pub use notifications::{
    is_tools_list_change, negotiate_protocol_version, notification_bus, spawn_notification_reader,
    McpNotification, McpNotificationTx,
};
pub use types::{McpPrompt, McpResource, McpTool, MCP_PROTOCOL_VERSIONS, RECONNECT_DELAYS_SECS};
