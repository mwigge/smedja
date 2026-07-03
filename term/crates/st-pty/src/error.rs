//! Error types produced by PTY operations.

use thiserror::Error;

/// Errors produced by PTY operations.
#[derive(Debug, Error)]
pub enum PtyError {
    /// PTY system call failed.
    #[error("pty error: {0}")]
    Pty(String),
    /// I/O error on the PTY master fd.
    #[error("pty I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Clipboard error.
    #[error("clipboard error: {0}")]
    Clipboard(String),
}
