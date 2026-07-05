//! NDJSON turn-streaming server.
//!
//! Accepts connections on a dedicated Unix socket (`<rpc_sock>.stream`).
//! Each connection reads a single JSON request `{"task_id":"..."}`, subscribes
//! to the Bellows dispatcher for that turn, replays any buffered events, then
//! forwards live events until the turn reaches a terminal state, at which point
//! it writes a `{"type":"done",...}` or `{"type":"error",...}` line and closes.
//!
//! Wire protocol (NDJSON — one JSON object per line):
//!
//! ```text
//! {"type":"delta","text":"Hello"}
//! {"type":"tool_call","name":"Bash","input":"ls"}
//! {"type":"done","output_tok":88,"input_tok":412,"elapsed_ms":4200}
//! {"type":"error","message":"stream timed out"}
//! ```
//!
//! The implementation is split by concern into private submodules:
//! * [`buffer`] — per-turn event buffering, TTL eviction, and the background
//!   subscriber/sweeper tasks.
//! * [`serve`] — the connection acceptor plus the per-connection replay/live
//!   forwarding loop.
//! * [`wire`] — [`TurnEvent`](smedja_bellows::TurnEvent) → NDJSON conversion.
//!
//! All previously public paths are preserved via the re-exports below.

mod buffer;
mod serve;
mod wire;

pub use buffer::{cleanup_turn, spawn_delta_buffer, DeltaStore, TurnBuffer};
pub use serve::{serve, stream_socket_path};
