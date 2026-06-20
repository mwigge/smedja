pub mod codec;
pub mod types;

pub use types::{Request, Response, Error as RpcError};

/// JSON-RPC 2.0 version string.
pub const JSONRPC: &str = "2.0";
