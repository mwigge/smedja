//! Public types shared between the LSP client and consumers.

use std::path::PathBuf;

/// LSP diagnostic severity (mirrors the protocol's 1–4 enum).
///
/// Variants are ordered so that `Error < Warning < Info < Hint`, enabling sort
/// to place the most important diagnostics first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

impl Severity {
    /// Convert from the LSP numeric severity (1=Error … 4=Hint).
    #[must_use]
    pub fn from_lsp(n: u64) -> Self {
        match n {
            1 => Self::Error,
            2 => Self::Warning,
            3 => Self::Info,
            _ => Self::Hint,
        }
    }

    /// Single-character label used in compact display.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "E",
            Self::Warning => "W",
            Self::Info => "I",
            Self::Hint => "H",
        }
    }
}

/// A single diagnostic from a language server.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// File path, relative to the workspace when possible.
    pub file: PathBuf,
    /// 1-based line number (converted from 0-based LSP).
    pub line: u32,
    /// 1-based column number.
    pub col: u32,
    pub severity: Severity,
    /// Optional error code (e.g. `"E0308"`, `"no-unused-vars"`).
    pub code: Option<String>,
    pub message: String,
}

/// Lifecycle state of a single language server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    Starting,
    Ready,
    Degraded(String),
}

/// Status line for one language server.
#[derive(Debug, Clone)]
pub struct ServerStatus {
    pub name: String,
    pub state: ServerState,
}

/// Combined snapshot of all active LSP servers and their current diagnostics.
#[derive(Debug, Clone, Default)]
pub struct LspSnapshot {
    pub servers: Vec<ServerStatus>,
    /// All diagnostics, sorted by severity then file then line.
    pub diagnostics: Vec<Diagnostic>,
}

impl LspSnapshot {
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }

    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count()
    }
}
