//! Shared state for ACP route handlers.

use std::sync::Arc;

use smedja_bellows::Dispatcher;
use smedja_ingot::IngotHandle;
use smedja_vault::Vault;
use tokio::sync::Mutex;

use super::event_buffer::EventBuffer;

/// Shared state for ACP route handlers.
#[derive(Clone)]
pub struct AcpState {
    pub ingot: IngotHandle,
    pub dispatcher: Arc<Dispatcher>,
    pub auth_token: String,
    /// Workspace root used by MCP server-mode tool dispatch.
    pub workspace: std::path::PathBuf,
    /// Vector store shared with MCP server-mode tool dispatch.
    pub vault: Arc<Mutex<Vault>>,
    /// Embedding backend shared with MCP server-mode tool dispatch.
    pub embedder: Arc<dyn crate::embedder_port::Embedder>,
    /// Per-turn SSE replay buffer — enables `Last-Event-ID` reconnect.
    pub replay: Arc<EventBuffer>,
    /// Monotonic counter for SSE event `id:` sequence numbers.
    pub next_seq: Arc<std::sync::atomic::AtomicU64>,
}

/// Builds an `AcpState` backed by in-memory stores for use in tests.
#[cfg(test)]
pub(crate) fn test_state() -> AcpState {
    let ingot = smedja_ingot::Ingot::open_in_memory().expect("in-memory ingot");
    AcpState {
        ingot: smedja_ingot::IngotHandle::new(ingot),
        dispatcher: Arc::new(Dispatcher::new(32)),
        auth_token: "test-token".to_owned(),
        workspace: std::env::temp_dir(),
        vault: Arc::new(tokio::sync::Mutex::new(
            smedja_vault::Vault::open_in_memory().expect("in-memory vault"),
        )),
        embedder: Arc::new(crate::embedder_port::FnvEmbedder::new()),
        replay: Arc::new(EventBuffer::new()),
        next_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
    }
}
