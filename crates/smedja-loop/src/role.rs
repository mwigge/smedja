//! Role definitions for the loop engine.
//!
//! Each role names a participating agent, the runner backend it targets, the
//! execution tier it requests, and whether it operates in read-only mode.

use serde::{Deserialize, Serialize};
use sha2::Digest as _;

pub use smedja_types::{Runner, Tier};

/// Data exposure boundaries for a loop role.
///
/// All fields default to `false` (deny) so that new roles are minimally
/// privileged unless explicitly granted access.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DataAccess {
    /// When `true`, the role may read files outside the workspace root.
    pub can_read_outside_workspace: bool,
    /// When `true`, the role may make outbound network calls.
    pub can_network: bool,
    /// When `true`, the role may write files outside the workspace root.
    pub can_write_outside_workspace: bool,
}

/// A single named participant in a loop pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopRole {
    /// Human-readable role name (e.g. `"orchestrator"`, `"reviewer"`).
    pub name: String,
    /// Runner backend for this role.
    pub runner: Runner,
    /// Execution tier for this role.
    pub tier: Tier,
    /// Optional model override (e.g. `"claude-sonnet-4-6"`).
    pub model: Option<String>,
    /// When `true`, this role must not write to the workspace.
    pub read_only: bool,
    /// Allowed tool names for this role (`[]` means no restriction).
    pub tools: Vec<String>,
    /// Deterministic role identity UUID, computed from loop ID and role name.
    ///
    /// Set to `Uuid::nil()` in [`LoopRole::defaults`]; callers must call
    /// [`LoopRole::compute_role_id`] and populate this field before recording
    /// audit events.
    #[serde(default)]
    pub role_id: uuid::Uuid,
    /// Data exposure boundaries for this role.
    ///
    /// All fields are `false` by default (deny-all).
    #[serde(default)]
    pub data_access: DataAccess,
    /// When set, the `task.parallel` fan-out resumes from this session's
    /// checkpoint history instead of starting fresh. `None` for loop-pipeline
    /// roles, which always start from the work envelope.
    #[serde(default)]
    pub resume_session_id: Option<String>,
}

impl LoopRole {
    /// Returns the default role table as per the loop engine spec.
    ///
    /// All roles are initialised with `role_id = Uuid::nil()`. Callers that
    /// need a stable identity must call [`LoopRole::compute_role_id`] and
    /// assign the result before emitting audit events.
    #[must_use]
    pub fn defaults() -> Vec<Self> {
        vec![
            Self {
                name: "orchestrator".into(),
                runner: Runner::Claude,
                tier: Tier::Deep,
                model: None,
                read_only: false,
                tools: vec![],
                role_id: uuid::Uuid::nil(),
                data_access: DataAccess::default(),
                resume_session_id: None,
            },
            Self {
                name: "proposer".into(),
                runner: Runner::Claude,
                tier: Tier::Fast,
                model: None,
                read_only: false,
                tools: vec![],
                role_id: uuid::Uuid::nil(),
                data_access: DataAccess::default(),
                resume_session_id: None,
            },
            Self {
                name: "tester".into(),
                runner: Runner::Local,
                tier: Tier::Local,
                model: None,
                read_only: false,
                tools: vec![],
                role_id: uuid::Uuid::nil(),
                data_access: DataAccess::default(),
                resume_session_id: None,
            },
            Self {
                // Executed plan phase — runs FIRST at the plan tier and produces
                // the slice list (writes/updates tasks.md). Bound to the owner's
                // planner model (fable) via `LoopRole::model` at turn time; the
                // runner here is left independent of the implementer/reviewer
                // runners so it never affects evaluator separation.
                name: "plan".into(),
                runner: Runner::Claude,
                tier: Tier::Deep,
                model: None,
                read_only: false,
                tools: vec![],
                role_id: uuid::Uuid::nil(),
                data_access: DataAccess::default(),
                resume_session_id: None,
            },
            Self {
                name: "implementer".into(),
                runner: Runner::Local,
                tier: Tier::Local,
                model: None,
                read_only: true,
                tools: vec![],
                role_id: uuid::Uuid::nil(),
                data_access: DataAccess::default(),
                resume_session_id: None,
            },
            Self {
                name: "reviewer".into(),
                runner: Runner::Minimax,
                tier: Tier::Fast,
                model: None,
                read_only: true,
                tools: vec![],
                role_id: uuid::Uuid::nil(),
                data_access: DataAccess::default(),
                resume_session_id: None,
            },
            Self {
                name: "fix".into(),
                runner: Runner::Local,
                tier: Tier::Local,
                model: None,
                read_only: false,
                tools: vec![],
                role_id: uuid::Uuid::nil(),
                data_access: DataAccess::default(),
                resume_session_id: None,
            },
        ]
    }

    /// Computes a deterministic role identity from `loop_id` and `role_name`.
    ///
    /// Uses the first 16 bytes of `SHA-256(loop_id + "-" + role_name)` to
    /// construct a UUID, providing a stable, loop-scoped identity for each
    /// participating role without requiring a separate `UUIDv5` namespace.
    #[must_use]
    pub fn compute_role_id(loop_id: &str, role_name: &str) -> uuid::Uuid {
        let mut h = sha2::Sha256::new();
        h.update(loop_id.as_bytes());
        h.update(b"-");
        h.update(role_name.as_bytes());
        let bytes = h.finalize();
        let mut arr = [0u8; 16];
        arr.copy_from_slice(&bytes[..16]);
        uuid::Uuid::from_bytes(arr)
    }

    /// Returns `true` when this role's runner differs from `other`'s runner.
    ///
    /// Evaluator separation requires that reviewer and implementer use different
    /// runner backends to prevent a single compromised runtime from self-approving.
    #[must_use]
    pub fn runner_differs_from(&self, other: &Self) -> bool {
        self.runner != other.runner
    }

    /// Builds a role for the `task.parallel` fan-out path from just a name and an
    /// optional resume session.
    ///
    /// The fan-out does not route by tier, so runner/tier default to
    /// [`Runner::Local`]/[`Tier::Local`] with deny-all data access. This is the
    /// single `LoopRole` type used by both the loop pipeline and `task.parallel`.
    #[must_use]
    pub fn for_parallel(name: impl Into<String>, resume_session_id: Option<String>) -> Self {
        Self {
            name: name.into(),
            runner: Runner::Local,
            tier: Tier::Local,
            model: None,
            read_only: false,
            tools: vec![],
            role_id: uuid::Uuid::nil(),
            data_access: DataAccess::default(),
            resume_session_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_contains_seven_roles_including_plan() {
        let defaults = LoopRole::defaults();
        assert_eq!(defaults.len(), 7);
        // The executed plan phase is part of the default role table.
        assert!(defaults.iter().any(|r| r.name == "plan"));
    }

    #[test]
    fn reviewer_and_implementer_differ_by_runner_in_defaults() {
        let defaults = LoopRole::defaults();
        let reviewer = defaults.iter().find(|r| r.name == "reviewer").unwrap();
        let implementer = defaults.iter().find(|r| r.name == "implementer").unwrap();
        // In the default table reviewer=Minimax, implementer=Local — separation holds.
        assert!(reviewer.runner_differs_from(implementer));
    }

    #[test]
    fn evaluator_separation_violation_detected() {
        let reviewer = LoopRole {
            name: "reviewer".into(),
            runner: Runner::Local,
            tier: Tier::Fast,
            model: None,
            read_only: true,
            tools: vec![],
            role_id: uuid::Uuid::nil(),
            data_access: DataAccess::default(),
            resume_session_id: None,
        };
        let implementer = LoopRole {
            name: "implementer".into(),
            runner: Runner::Local,
            tier: Tier::Local,
            model: None,
            read_only: false,
            tools: vec![],
            role_id: uuid::Uuid::nil(),
            data_access: DataAccess::default(),
            resume_session_id: None,
        };
        // Both use Local — separation is violated.
        assert!(!reviewer.runner_differs_from(&implementer));
    }

    #[test]
    fn compute_role_id_is_deterministic() {
        let id1 = LoopRole::compute_role_id("loop-abc", "reviewer");
        let id2 = LoopRole::compute_role_id("loop-abc", "reviewer");
        assert_eq!(id1, id2);
    }

    #[test]
    fn compute_role_id_differs_across_roles() {
        let reviewer_id = LoopRole::compute_role_id("loop-abc", "reviewer");
        let implementer_id = LoopRole::compute_role_id("loop-abc", "implementer");
        assert_ne!(reviewer_id, implementer_id);
    }

    #[test]
    fn compute_role_id_differs_across_loops() {
        let id1 = LoopRole::compute_role_id("loop-abc", "reviewer");
        let id2 = LoopRole::compute_role_id("loop-xyz", "reviewer");
        assert_ne!(id1, id2);
    }

    #[test]
    fn data_access_defaults_to_deny_all() {
        let access = DataAccess::default();
        assert!(!access.can_read_outside_workspace);
        assert!(!access.can_network);
        assert!(!access.can_write_outside_workspace);
    }

    #[test]
    fn defaults_roles_have_deny_all_data_access() {
        for role in LoopRole::defaults() {
            assert!(
                !role.data_access.can_write_outside_workspace,
                "role '{}' must have write-outside-workspace denied by default",
                role.name
            );
        }
    }

    #[test]
    fn runner_serialises_to_lowercase() {
        let json = serde_json::to_string(&Runner::Claude).unwrap();
        assert_eq!(json, r#""claude""#);
        let json = serde_json::to_string(&Runner::Minimax).unwrap();
        assert_eq!(json, r#""minimax""#);
    }

    #[test]
    fn tier_serialises_to_lowercase() {
        let json = serde_json::to_string(&Tier::Deep).unwrap();
        assert_eq!(json, r#""deep""#);
    }
}
