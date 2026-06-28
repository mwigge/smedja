/// Supported development methodology modes.
///
/// TDD and clean-code discipline are no longer selectable modes — they are the
/// always-on foundational discipline (steering + backstops). The remaining modes
/// are the selectable lifecycle/gate concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Spec-first development — changes must reference a specification.
    Spec,
    /// Clean gate — hard blocker on `unwrap`/`expect` and debug output in
    /// production code.
    Clean,
}

/// Configuration for an active methodology session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    /// The set of methodology modes active in this session.
    pub modes: Vec<Mode>,
}

/// A methodology gate violation.
///
/// Returned from a gate check when the diff fails the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodologyViolation {
    /// The name of the gate that raised this violation.
    pub gate: &'static str,
    /// A human-readable description of what triggered the violation.
    pub message: String,
}

impl MethodologyViolation {
    /// Creates a new [`MethodologyViolation`].
    #[must_use]
    pub fn new(gate: &'static str, message: impl Into<String>) -> Self {
        Self {
            gate,
            message: message.into(),
        }
    }
}

/// The result type for all methodology gate checks.
pub type GateResult = Result<(), MethodologyViolation>;

/// Composite quality score for a single turn's diff.
///
/// Four gates each contribute 25 points for a maximum of 100:
///
/// | Gate | Points | Fails when |
/// |---|---|---|
/// | TDD backstop | 25 | [`tdd::TddVerdict::Advisory`] |
/// | Clean gate | 25 | any violation |
/// | File size | 25 | any file over threshold |
/// | Skill inject | 25 | any missing skill advisory |
///
/// A score ≥ 60 is green; < 60 is advisory (yellow); two consecutive turns
/// below 60 trigger the `CoworkGate` interrupt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityScore {
    /// Composite 0–100 score.
    pub score: u8,
    /// Whether the TDD gate passed (25 pts).
    pub tdd_pass: bool,
    /// Whether the clean gate passed (25 pts).
    pub clean_pass: bool,
    /// Whether the file-size gate passed (no advisories, 25 pts).
    pub file_size_pass: bool,
    /// Whether the skill-inject gate passed (no advisories, 25 pts).
    pub skill_inject_pass: bool,
}

impl QualityScore {
    /// Returns `true` when the score is at or above the green threshold (60).
    #[must_use]
    pub fn is_green(&self) -> bool {
        self.score >= 60
    }
}
