/// Maximum recursion depth for resumable role sessions.
///
/// smdjad refuses to resume a session that already has this many compaction
/// checkpoints (`turn_n = -1`) to prevent infinite spawning.
pub const MAX_ROLE_DEPTH: u8 = 4;

/// The agent role that determines routing behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    /// Implements features and fixes bugs.
    Impl,
    /// Writes and validates tests.
    Test,
    /// Reviews code for correctness and style.
    Review,
    /// Handles site reliability and observability.
    Sre,
    /// Coordinates and orchestrates multi-agent workflows.
    Orchestrator,
}

pub use smedja_types::{Complexity, Runner, Tier};

/// The resolved routing destination for a role × complexity combination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The model runner backend to use.
    pub runner: Runner,
    /// The execution tier to request.
    pub tier: Tier,
    /// Optional model override (e.g. `"claude-sonnet-4-6"`). `None` uses the runner default.
    pub model: Option<String>,
}

/// A fully resolved routing decision: the chosen destination plus the inputs
/// and rationale that produced it.
///
/// This is the "`ModelSpec`" from the design — it captures not just the
/// `(Runner, Tier, model)` destination but also the `Complexity` that was used
/// to reach it and a short human-readable `rationale` explaining the choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingDecision {
    runner: Runner,
    tier: Tier,
    model: Option<String>,
    complexity: Complexity,
    rationale: String,
}

impl RoutingDecision {
    /// Creates a new routing decision.
    #[must_use]
    pub fn new(
        runner: Runner,
        tier: Tier,
        model: Option<String>,
        complexity: Complexity,
        rationale: String,
    ) -> Self {
        Self {
            runner,
            tier,
            model,
            complexity,
            rationale,
        }
    }

    /// Returns the chosen runner backend.
    #[must_use]
    pub fn runner(&self) -> Runner {
        self.runner
    }

    /// Returns the chosen execution tier.
    #[must_use]
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// Returns the optional model override. `None` uses the runner default.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Returns the complexity that was used to reach this decision.
    #[must_use]
    pub fn complexity(&self) -> Complexity {
        self.complexity
    }

    /// Returns a short human-readable explanation of the decision.
    #[must_use]
    pub fn rationale(&self) -> &str {
        &self.rationale
    }
}
