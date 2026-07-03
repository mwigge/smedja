//! OAuth 2.0 PKCE flow for MCP HTTP server authentication.
//!
//! Implements the Authorization Code + PKCE (S256) flow as required by the MCP
//! specification for HTTP-based tool servers: a code verifier/challenge, a
//! loopback redirect listener with `state` validation, a token exchange, and a
//! refresh-token grant. Tokens are persisted by [`TokenStore`].
//!
//! At-rest token confidentiality relies on filesystem permissions (0600);
//! AES-256-GCM encryption is a separate hardening item.

mod flow;
mod pkce;
mod store;
mod token;

pub use flow::{refresh_token, start_pkce};
pub use store::TokenStore;
pub use token::{PkceError, Token};
