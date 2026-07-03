//! ACP HTTP server — Agent Coordination Protocol over HTTP.
//!
//! Activated by `SMEDJA_ACP_PORT` environment variable (default: disabled).
//! Routes proxy into smdjad's ingot and dispatcher directly.

mod event_buffer;
mod prompt;
mod router;
mod session;
mod sse;
mod state;

pub use event_buffer::EventBuffer;
pub use router::build_acp_router;
pub use state::AcpState;
