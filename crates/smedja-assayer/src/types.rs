/// Maximum recursion depth for resumable role sessions.
///
/// smdjad refuses to resume a session that already has this many compaction
/// checkpoints (`turn_n = -1`) to prevent infinite spawning.
pub const MAX_ROLE_DEPTH: u8 = 4;

/// The agent role that determines routing behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    /// Implements features and fixes bugs (the "Code" role).
    Impl,
    /// Designs architecture and writes an implementation plan; makes no edits.
    Plan,
    /// Gathers information from the web, PDFs, and images. Read-only + network.
    Research,
    /// Troubleshoots, traces, and fixes issues.
    Debug,
    /// Answers questions about the codebase without touching files.
    Ask,
    /// Writes and validates tests.
    Test,
    /// Reviews code for correctness and style.
    Review,
    /// Handles site reliability and observability.
    Sre,
    /// Database/SQL work: schema, queries, migrations.
    Data,
    /// Infrastructure-as-code (terraform/k8s/…). High-risk: apply/destroy ops.
    Iac,
    /// Coordinates and orchestrates multi-agent workflows.
    Orchestrator,
    /// Reads and searches the codebase; never mutates. Allow-list: read, glob, grep, list.
    Search,
}

impl AgentRole {
    /// Whether this role is read-only: it must never mutate the workspace
    /// (write/edit/shell), regardless of the session permission mode. Used to
    /// hard-deny mutating tool calls for analysis/planning/research roles.
    #[must_use]
    pub fn is_read_only(self) -> bool {
        matches!(
            self,
            AgentRole::Plan
                | AgentRole::Research
                | AgentRole::Review
                | AgentRole::Ask
                | AgentRole::Orchestrator
                | AgentRole::Search
        )
    }

    /// Capability tags the role needs from its routed client (AgentField-style).
    /// A router should prefer a client that advertises these — e.g. Research
    /// needs `web`/`pdf`/`vision`, which today only the external CLIs provide.
    #[must_use]
    pub fn capabilities(self) -> &'static [&'static str] {
        match self {
            AgentRole::Research => &["web", "pdf", "vision"],
            AgentRole::Data => &["sql"],
            AgentRole::Iac => &["iac"],
            _ => &[],
        }
    }

    /// High-risk roles whose mutations are always confirmed (never auto-approved),
    /// because they perform dangerous, hard-to-reverse operations — e.g.
    /// Infra-as-Code `apply`/`destroy`.
    #[must_use]
    pub fn is_high_risk(self) -> bool {
        matches!(self, AgentRole::Iac)
    }

    /// Lowercase identifier for the role (used for routing rationale, role-skill
    /// file lookup, etc.).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            AgentRole::Impl => "impl",
            AgentRole::Plan => "plan",
            AgentRole::Research => "research",
            AgentRole::Debug => "debug",
            AgentRole::Ask => "ask",
            AgentRole::Test => "test",
            AgentRole::Review => "review",
            AgentRole::Sre => "sre",
            AgentRole::Data => "data",
            AgentRole::Iac => "iac",
            AgentRole::Orchestrator => "orchestrator",
            AgentRole::Search => "search",
        }
    }
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
    /// Tool whitelist from `agents.toml`; empty means all tools are allowed.
    pub tools: Vec<String>,
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
