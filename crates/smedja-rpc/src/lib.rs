pub mod codec;
pub mod types;

pub use types::{Error as RpcError, Request, Response};

/// JSON-RPC 2.0 version string.
pub const JSONRPC: &str = "2.0";
