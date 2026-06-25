//! `smedja-lsp` — lightweight LSP client crate.
//!
//! Detects language servers available on `$PATH`, spawns them for a workspace
//! root, and aggregates their `textDocument/publishDiagnostics` notifications
//! into a shared `LspSnapshot` exposed via `tokio::sync::watch`.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use smedja_lsp::LspManager;
//!
//! async fn example() {
//!     let manager = LspManager::new();
//!     manager.start(std::env::current_dir().unwrap());
//!     let mut rx = manager.subscribe();
//!     rx.changed().await.unwrap();
//!     let snap = rx.borrow().clone();
//!     println!("{} errors", snap.error_count());
//! }
//! ```

mod client;
mod manager;
pub mod types;

pub use manager::LspManager;
pub use types::{Diagnostic, LspSnapshot, ServerState, ServerStatus, Severity};
