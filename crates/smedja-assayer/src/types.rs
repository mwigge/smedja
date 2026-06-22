/// Maximum recursion depth for resumable role sessions.
///
/// smdjad refuses to resume a session that already has this many compaction
/// checkpoints (`turn_n = -1`) to prevent infinite spawning.
pub const MAX_ROLE_DEPTH: u8 = 4;

/// A role entry for `task.parallel`, optionally resuming a prior session.
#[derive(Debug, Clone)]
pub struct LoopRole {
    /// The role name (e.g. `"impl"`, `"test"`).
    pub name: String,
    /// If set, the parallel worker resumes from this session's checkpoint history.
    pub resume_session_id: Option<String>,
}

/// The agent role that determines routing behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
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

/// Estimated complexity of the task being assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Complexity {
    /// Trivial change: config tweak, one-liner fix, doc update.
    Simple,
    /// Moderate change: single module, a few functions, straightforward logic.
    Coding,
    /// High-effort change: cross-module, design-sensitive, or multi-step.
    Complex,
}

/// The execution tier that controls latency vs. capability trade-offs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tier {
    /// Low latency, small context window, cheap.
    Fast,
    /// Local model running on device — no cloud egress.
    Local,
    /// High capability, large context window, higher latency.
    Deep,
}

/// The model runner backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Runner {
    /// Anthropic Claude (cloud).
    Claude,
    /// Local model via smedja-native.
    Local,
    /// `OpenAI` Codex (cloud).
    Codex,
    /// GitHub Copilot (cloud).
    Copilot,
}

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
