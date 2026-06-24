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
