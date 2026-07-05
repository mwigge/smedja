//! Error type for `smedja-plugins`.

use std::path::PathBuf;

/// Errors produced by skill registry operations.
#[derive(Debug, thiserror::Error)]
pub enum PluginsError {
    /// A skill with the given name already exists at the specified path.
    #[error("skill `{name}` already exists at {path}")]
    AlreadyExists { name: String, path: PathBuf },

    /// No skill with the given name was found in the registry.
    #[error("skill `{name}` not found")]
    NotFound { name: String },

    /// The skill name is not a single normal path component and would escape
    /// the registry directory (e.g. contains `/`, `..`, `.`, is absolute, or
    /// is empty).
    #[error("invalid skill name `{name}`: must be a single path component")]
    InvalidName { name: String },

    /// The skill file at the specified path could not be parsed.
    #[error("failed to parse skill at {path}: {reason}")]
    ParseFailed { path: PathBuf, reason: String },

    /// An I/O error occurred.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}
