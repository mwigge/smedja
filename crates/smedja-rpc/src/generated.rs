// @generated — do not edit by hand; regenerate with `cargo xtask gen-rpc-types`
#![allow(clippy::all, unused_imports)]
use serde::{Deserialize, Serialize};

/// Opaque identifier for an interactive session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    /// Creates a new [`SessionId`] from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque identifier for a single turn within a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnId(pub String);

impl TurnId {
    /// Creates a new [`TurnId`] from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque identifier for an async task tracked in smedja-ingot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

impl TaskId {
    /// Creates a new [`TaskId`] from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_roundtrip_serde() {
        let id = SessionId::new("sess-abc-123");
        let json = serde_json::to_string(&id).unwrap();
        let back: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn turn_id_roundtrip_serde() {
        let id = TurnId::new("turn-xyz-456");
        let json = serde_json::to_string(&id).unwrap();
        let back: TurnId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn task_id_roundtrip_serde() {
        let id = TaskId::new("task-qrs-789");
        let json = serde_json::to_string(&id).unwrap();
        let back: TaskId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn session_id_display() {
        let id = SessionId::new("sess-display");
        assert_eq!(id.to_string(), "sess-display");
    }

    #[test]
    fn turn_id_display() {
        let id = TurnId::new("turn-display");
        assert_eq!(id.to_string(), "turn-display");
    }

    #[test]
    fn task_id_display() {
        let id = TaskId::new("task-display");
        assert_eq!(id.to_string(), "task-display");
    }
}
