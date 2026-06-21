//! Role definitions for the loop engine.
//!
//! Each role names a participating agent, the runner backend it targets, the
//! execution tier it requests, and whether it operates in read-only mode.

use serde::{Deserialize, Serialize};

/// The model runner backend for a loop role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runner {
    /// Anthropic Claude (cloud).
    Claude,
    /// Local model via rs-llmctl.
    Local,
    /// `OpenAI` Codex (cloud).
    Codex,
    /// `MiniMax` (cloud).
    Minimax,
    /// Berget (cloud).
    Berget,
}

/// Execution tier controlling latency vs. capability trade-offs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Low latency, small context window, cheap.
    Fast,
    /// Local model — no cloud egress.
    Local,
    /// High capability, large context window, higher latency.
    Deep,
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
}

impl LoopRole {
    /// Returns the default role table as per the loop engine spec.
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
            },
            Self {
                name: "proposer".into(),
                runner: Runner::Claude,
                tier: Tier::Fast,
                model: None,
                read_only: false,
                tools: vec![],
            },
            Self {
                name: "tester".into(),
                runner: Runner::Local,
                tier: Tier::Local,
                model: None,
                read_only: false,
                tools: vec![],
            },
            Self {
                name: "implementer".into(),
                runner: Runner::Local,
                tier: Tier::Local,
                model: None,
                read_only: true,
                tools: vec![],
            },
            Self {
                name: "reviewer".into(),
                runner: Runner::Minimax,
                tier: Tier::Fast,
                model: None,
                read_only: true,
                tools: vec![],
            },
            Self {
                name: "fix".into(),
                runner: Runner::Local,
                tier: Tier::Local,
                model: None,
                read_only: false,
                tools: vec![],
            },
        ]
    }

    /// Returns `true` when this role's runner differs from `other`'s runner.
    ///
    /// Evaluator separation requires that reviewer and implementer use different
    /// runner backends to prevent a single compromised runtime from self-approving.
    #[must_use]
    pub fn runner_differs_from(&self, other: &Self) -> bool {
        self.runner != other.runner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_contains_six_roles() {
        assert_eq!(LoopRole::defaults().len(), 6);
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
        };
        let implementer = LoopRole {
            name: "implementer".into(),
            runner: Runner::Local,
            tier: Tier::Local,
            model: None,
            read_only: false,
            tools: vec![],
        };
        // Both use Local — separation is violated.
        assert!(!reviewer.runner_differs_from(&implementer));
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
