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
//! Structure:
//! - [`buffer`]: per-turn NDJSON event buffer and its background subscriber.
//! - [`convert`]: conversion of Bellows `TurnEvent`s to NDJSON wire lines.
//! - [`connection`]: stream socket listener and per-connection handling.

mod buffer;
mod connection;
mod convert;

pub use buffer::{cleanup_turn, spawn_delta_buffer, DeltaStore};
pub use connection::{serve, stream_socket_path};
