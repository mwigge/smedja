//! Canonical shared types for the smedja workspace.
//!
//! Provides [`Runner`], [`Tier`], and [`Complexity`] as the single source of
//! truth for all crates that need to interoperate on model routing.

use serde::{Deserialize, Serialize};

/// The model runner backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runner {
    /// Anthropic Claude (cloud).
    Claude,
    /// `OpenAI` Codex (cloud).
    Codex,
    /// Local model running on device — no cloud egress.
    Local,
    /// GitHub Copilot (cloud).
    Copilot,
    /// `MiniMax` (cloud).
    Minimax,
    /// Berget (cloud).
    Berget,
}

/// The execution tier that controls latency vs. capability trade-offs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Low latency, small context window, cheap.
    Fast,
    /// Local model running on device — no cloud egress.
    Local,
    /// High capability, large context window, higher latency.
    Deep,
}

/// Estimated complexity of the task being assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Complexity {
    /// Trivial change: config tweak, one-liner fix, doc update.
    Simple,
    /// Moderate change: single module, a few functions, straightforward logic.
    Coding,
    /// High-effort change: cross-module, design-sensitive, or multi-step.
    Complex,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runner_serde_roundtrip() {
        for runner in [
            Runner::Claude,
            Runner::Codex,
            Runner::Local,
            Runner::Copilot,
            Runner::Minimax,
            Runner::Berget,
        ] {
            let json = serde_json::to_string(&runner).expect("serialise runner");
            let back: Runner = serde_json::from_str(&json).expect("deserialise runner");
            assert_eq!(runner, back);
        }
    }

    #[test]
    fn tier_serde_roundtrip() {
        for tier in [Tier::Fast, Tier::Local, Tier::Deep] {
            let json = serde_json::to_string(&tier).expect("serialise tier");
            let back: Tier = serde_json::from_str(&json).expect("deserialise tier");
            assert_eq!(tier, back);
        }
    }

    #[test]
    fn complexity_serde_roundtrip() {
        for complexity in [Complexity::Simple, Complexity::Coding, Complexity::Complex] {
            let json = serde_json::to_string(&complexity).expect("serialise complexity");
            let back: Complexity = serde_json::from_str(&json).expect("deserialise complexity");
            assert_eq!(complexity, back);
        }
    }
}
