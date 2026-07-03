//! Shell integration block markers (OSC 133 + heuristic).

/// The kind of a block marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerKind {
    /// OSC 133 A — prompt start.
    PromptStart,
    /// OSC 133 B — command start.
    CommandStart,
    /// OSC 133 C — command executed.
    CommandExecuted,
    /// OSC 133 D — command done.  The payload may contain the exit code.
    CommandDone { exit_code: Option<i32> },
    /// Heuristic prompt detection (PS1 pattern match).
    PromptHeuristic,
    /// OSC 7 — current working directory notification.
    Osc7Cwd { path: String },
}

/// Marks a row as a shell integration boundary.
#[derive(Debug, Clone)]
pub struct BlockMarker {
    /// What kind of boundary this is.
    pub kind: MarkerKind,
    /// The terminal row where the marker was emitted.
    pub row: u16,
}
