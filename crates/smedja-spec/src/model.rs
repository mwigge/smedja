//! Data model for the native OpenSpec engine.
//!
//! The core primitives are [`Requirement`] (a `SHALL`/`MUST` statement plus its
//! scenarios), [`Scenario`] (a `GIVEN`/`WHEN`/`THEN` case), [`Delta`] (the
//! `ADDED`/`MODIFIED`/`REMOVED` requirement buckets a change proposes for one
//! capability), and [`Spec`] (a capability's source-of-truth requirement set).

use serde::{Deserialize, Serialize};

/// A single `GIVEN`/`WHEN`/`THEN` scenario attached to a requirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scenario {
    /// Scenario name (the text after `#### Scenario:`).
    pub name: String,
    /// The scenario body — the `GIVEN`/`WHEN`/`THEN` lines, trimmed.
    pub body: String,
}

impl Scenario {
    /// Creates a scenario with the given `name` and `body`.
    pub fn new(name: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            body: body.into(),
        }
    }
}

/// A requirement: a `SHALL`/`MUST` statement plus zero or more scenarios.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    /// Requirement name (the text after `### Requirement:`).
    pub name: String,
    /// The requirement statement body (everything between the header and the
    /// first scenario), trimmed.
    pub text: String,
    /// The scenarios that make the requirement testable.
    pub scenarios: Vec<Scenario>,
}

impl Requirement {
    /// Creates a requirement with no scenarios.
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            text: text.into(),
            scenarios: Vec::new(),
        }
    }

    /// Returns whether the requirement's statement asserts `SHALL` or `MUST`
    /// (case-insensitive) — the normative keyword an OpenSpec requirement needs.
    #[must_use]
    pub fn is_normative(&self) -> bool {
        let upper = self.text.to_ascii_uppercase();
        upper.contains("SHALL") || upper.contains("MUST")
    }
}

/// The three delta operations a change can apply to a capability's requirements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeltaOp {
    /// A brand-new requirement appended to the capability.
    Added,
    /// A requirement whose definition replaces the existing same-named one.
    Modified,
    /// A requirement removed from the capability.
    Removed,
}

impl DeltaOp {
    /// The section header this operation is written under in a delta spec.
    #[must_use]
    pub fn header(self) -> &'static str {
        match self {
            DeltaOp::Added => "## ADDED Requirements",
            DeltaOp::Modified => "## MODIFIED Requirements",
            DeltaOp::Removed => "## REMOVED Requirements",
        }
    }
}

/// The delta a change proposes for a single capability.
///
/// A delta lives at `openspec/changes/<name>/specs/<capability>/spec.md` and is
/// merged into the capability's source spec on archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Delta {
    /// The capability this delta targets.
    pub capability: String,
    /// Requirements appended to the capability.
    pub added: Vec<Requirement>,
    /// Requirements whose definitions replace existing same-named ones.
    pub modified: Vec<Requirement>,
    /// Requirements removed from the capability.
    pub removed: Vec<Requirement>,
}

impl Delta {
    /// Creates an empty delta for `capability`.
    pub fn new(capability: impl Into<String>) -> Self {
        Self {
            capability: capability.into(),
            ..Self::default()
        }
    }

    /// Returns whether the delta proposes no changes at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }
}

/// A capability's source-of-truth spec (`openspec/specs/<capability>/spec.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    /// The capability name (its directory under `specs/`).
    pub capability: String,
    /// Free-form header text before the first requirement (title, purpose, …),
    /// preserved verbatim across merges.
    pub preamble: String,
    /// The capability's requirements.
    pub requirements: Vec<Requirement>,
}

impl Spec {
    /// Creates a spec for a new `capability` with a default header and no
    /// requirements.
    #[must_use]
    pub fn new_empty(capability: &str) -> Self {
        Self {
            capability: capability.to_owned(),
            preamble: format!("# {capability} Specification\n\n## Requirements"),
            requirements: Vec::new(),
        }
    }

    /// Returns a mutable reference to the requirement named `name`, if present.
    pub fn requirement_mut(&mut self, name: &str) -> Option<&mut Requirement> {
        self.requirements.iter_mut().find(|r| r.name == name)
    }

    /// Returns whether a requirement named `name` exists.
    #[must_use]
    pub fn has_requirement(&self, name: &str) -> bool {
        self.requirements.iter().any(|r| r.name == name)
    }
}
