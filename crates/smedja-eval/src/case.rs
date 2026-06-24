//! Eval-case format and the suite loader.
//!
//! An [`EvalCase`] is a versioned envelope authored as data (JSON or TOML) and
//! deserialised by [`load_suite`] from a directory of case files plus a sibling
//! `suite.toml` that carries the scoring configuration and pass threshold. The
//! envelope is intentionally `serde`-derived so the corpus can be extended
//! without recompiling the crate.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use smedja_assayer::AgentRole;
use smedja_types::{Complexity, Runner, Tier};

/// The only case-envelope version this crate understands.
///
/// [`load_suite`] rejects any case file declaring a different version rather
/// than silently mis-scoring it.
pub const SUPPORTED_VERSION: u32 = 1;

/// Errors raised while loading a suite from disk.
#[derive(Debug, thiserror::Error)]
pub enum CaseError {
    /// A case file declared a `version` this crate does not support.
    #[error("unsupported case version {version} in {path} (supported: {supported})")]
    UnsupportedVersion {
        /// The offending file.
        path: PathBuf,
        /// The version the file declared.
        version: u32,
        /// The version this crate supports.
        supported: u32,
    },
    /// A case file could not be parsed.
    #[error("failed to parse {path}: {message}")]
    Parse {
        /// The offending file.
        path: PathBuf,
        /// The underlying parse error message.
        message: String,
    },
    /// A required file or directory could not be read.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A case file used an extension the loader does not recognise.
    #[error("unsupported case file extension for {path} (expected .json or .toml)")]
    UnsupportedExtension {
        /// The offending file.
        path: PathBuf,
    },
}

/// The agent role under evaluation.
///
/// A serialisable mirror of [`smedja_assayer::AgentRole`] (which is not
/// `serde`-derived) so routing cases can name a role in data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvalRole {
    /// Implements features and fixes bugs.
    Impl,
    /// Writes and validates tests.
    Test,
    /// Reviews code for correctness and style.
    Review,
    /// Handles site reliability and observability.
    Sre,
    /// Coordinates multi-agent workflows.
    Orchestrator,
}

impl From<EvalRole> for AgentRole {
    fn from(role: EvalRole) -> Self {
        match role {
            EvalRole::Impl => Self::Impl,
            EvalRole::Test => Self::Test,
            EvalRole::Review => Self::Review,
            EvalRole::Sre => Self::Sre,
            EvalRole::Orchestrator => Self::Orchestrator,
        }
    }
}

/// The discriminant for which surface a case evaluates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaseKind {
    /// A deterministic routing case scored by exact match.
    Routing,
    /// A graded agent/loop case scored by deterministic and/or rubric scorers.
    Agent,
}

/// The labelled input for a case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Input {
    /// A routing input: a `(role, complexity)` pair.
    Routing {
        /// The role to route.
        role: EvalRole,
        /// The estimated complexity.
        complexity: Complexity,
    },
    /// An agent input: a free-text scenario describing the change to drive.
    Agent {
        /// The change/slice scenario for the loop driver.
        scenario: String,
    },
}

/// The expected destination for a routing case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingExpectation {
    /// The expected runner backend.
    pub runner: Runner,
    /// The expected execution tier.
    pub tier: Tier,
}

/// The expected outcome for an agent case.
///
/// Either a deterministic predicate over the loop outcome or a rubric judged by
/// the injected `Judge`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentExpectation {
    /// A named deterministic predicate over the loop outcome.
    Deterministic {
        /// The terminal `LoopState` (lowercase) the outcome must reach, if set.
        #[serde(default)]
        final_state: Option<String>,
        /// The minimum number of slices that must complete, if set.
        #[serde(default)]
        min_slices_completed: Option<u64>,
    },
    /// A rubric judged by the injected `Judge` over produced output.
    ///
    /// Carries the rubric prompt presented to the judge.
    Rubric(String),
}

/// The typed expectation for a case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Expectation {
    /// A routing destination scored by exact match.
    Routing(RoutingExpectation),
    /// An agent outcome scored by a deterministic predicate or a rubric.
    Agent(AgentExpectation),
}

/// A single eval case: a versioned envelope carrying its input and expectation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalCase {
    /// The envelope version. Must equal [`SUPPORTED_VERSION`].
    pub version: u32,
    /// A stable identifier for the case, used in the report.
    pub id: String,
    /// A free-text description of what the case checks.
    #[serde(default)]
    pub description: String,
    /// The surface this case evaluates.
    pub kind: CaseKind,
    /// The labelled input.
    pub input: Input,
    /// The expected result.
    pub expectation: Expectation,
    /// How many times to run the case (graded cases only). Defaults to 1.
    #[serde(default = "default_repetitions")]
    pub repetitions: u32,
    /// How many runs must pass for the case to pass. Defaults to 1.
    #[serde(default = "default_pass_threshold_k")]
    pub pass_threshold_k: u32,
}

fn default_repetitions() -> u32 {
    1
}

fn default_pass_threshold_k() -> u32 {
    1
}

/// A file of one or more cases.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CaseFile {
    /// A file holding a single case.
    Single(EvalCase),
    /// A file holding `{ "cases": [...] }`.
    Many {
        /// The cases declared in the file.
        cases: Vec<EvalCase>,
    },
}

/// The scoring configuration carried by a suite's `suite.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuiteConfig {
    /// A human-readable suite name, used as the metrics attribute.
    pub name: String,
    /// The pass-rate threshold the suite must meet, in `[0.0, 1.0]`.
    pub pass_threshold: f64,
}

/// A loaded suite: its configuration plus its cases.
#[derive(Debug, Clone)]
pub struct Suite {
    /// The suite configuration.
    pub config: SuiteConfig,
    /// The cases in the suite.
    pub cases: Vec<EvalCase>,
}

impl Suite {
    /// Returns the case with the given `id`, if any.
    #[must_use]
    pub fn case(&self, id: &str) -> Option<&EvalCase> {
        self.cases.iter().find(|c| c.id == id)
    }
}

/// Validates a single deserialised case, rejecting unsupported versions.
fn validate_case(case: &EvalCase, path: &Path) -> Result<(), CaseError> {
    if case.version != SUPPORTED_VERSION {
        return Err(CaseError::UnsupportedVersion {
            path: path.to_path_buf(),
            version: case.version,
            supported: SUPPORTED_VERSION,
        });
    }
    Ok(())
}

/// Parses one case file (`.json` or `.toml`) into its cases.
fn parse_case_file(path: &Path, contents: &str) -> Result<Vec<EvalCase>, CaseError> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let file: CaseFile = match extension.as_deref() {
        Some("json") => serde_json::from_str(contents).map_err(|e| CaseError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?,
        Some("toml") => toml::from_str(contents).map_err(|e| CaseError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?,
        _ => {
            return Err(CaseError::UnsupportedExtension {
                path: path.to_path_buf(),
            })
        }
    };
    let cases = match file {
        CaseFile::Single(case) => vec![case],
        CaseFile::Many { cases } => cases,
    };
    for case in &cases {
        validate_case(case, path)?;
    }
    Ok(cases)
}

/// Loads a suite from `dir`: every `.json`/`.toml` case file plus the required
/// `suite.toml` configuration.
///
/// # Errors
///
/// Returns a [`CaseError`] if `suite.toml` is missing or malformed, if any case
/// file cannot be parsed, or if any case declares an unsupported version. No
/// partial suite is returned on error.
#[must_use = "the loaded suite must be inspected or run"]
pub fn load_suite(dir: &Path) -> Result<Suite, CaseError> {
    let suite_toml = dir.join("suite.toml");
    let config_contents = std::fs::read_to_string(&suite_toml)?;
    let config: SuiteConfig = toml::from_str(&config_contents).map_err(|e| CaseError::Parse {
        path: suite_toml.clone(),
        message: e.to_string(),
    })?;

    // Collect case files in a stable (sorted) order for deterministic reports.
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("suite.toml") {
            continue;
        }
        if let Some("json" | "toml") = path.extension().and_then(|e| e.to_str()) {
            entries.push(path);
        }
    }
    entries.sort();

    let mut cases = Vec::new();
    for path in &entries {
        let contents = std::fs::read_to_string(path)?;
        cases.extend(parse_case_file(path, &contents)?);
    }

    Ok(Suite { config, cases })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialises_a_routing_case() {
        let json = r#"{
            "version": 1,
            "id": "review-coding",
            "description": "Review at coding complexity routes to claude/deep",
            "kind": "routing",
            "input": { "role": "review", "complexity": "coding" },
            "expectation": { "runner": "claude", "tier": "deep" }
        }"#;
        let case: EvalCase = serde_json::from_str(json).expect("parse routing case");
        assert_eq!(case.version, 1);
        assert_eq!(case.id, "review-coding");
        assert_eq!(case.kind, CaseKind::Routing);
        assert_eq!(
            case.input,
            Input::Routing {
                role: EvalRole::Review,
                complexity: Complexity::Coding,
            }
        );
        assert_eq!(
            case.expectation,
            Expectation::Routing(RoutingExpectation {
                runner: Runner::Claude,
                tier: Tier::Deep,
            })
        );
    }

    #[test]
    fn deserialises_an_agent_case() {
        let json = r#"{
            "version": 1,
            "id": "complete-loop",
            "kind": "agent",
            "input": { "scenario": "add a one-line fix" },
            "expectation": {
                "deterministic": { "final_state": "complete", "min_slices_completed": 1 }
            }
        }"#;
        let case: EvalCase = serde_json::from_str(json).expect("parse agent case");
        assert_eq!(case.kind, CaseKind::Agent);
        assert_eq!(
            case.input,
            Input::Agent {
                scenario: "add a one-line fix".to_owned(),
            }
        );
        assert_eq!(
            case.expectation,
            Expectation::Agent(AgentExpectation::Deterministic {
                final_state: Some("complete".to_owned()),
                min_slices_completed: Some(1),
            })
        );
    }

    #[test]
    fn rejects_unknown_version() {
        let json = r#"{
            "version": 99,
            "id": "future-case",
            "kind": "routing",
            "input": { "role": "impl", "complexity": "simple" },
            "expectation": { "runner": "local", "tier": "local" }
        }"#;
        let path = Path::new("future.json");
        let err = parse_case_file(path, json).expect_err("must reject unknown version");
        assert!(matches!(
            err,
            CaseError::UnsupportedVersion { version: 99, .. }
        ));
    }

    #[test]
    fn loads_suite_from_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("suite.toml"),
            "name = \"routing\"\npass_threshold = 1.0\n",
        )
        .expect("write suite.toml");
        std::fs::write(
            dir.path().join("review.json"),
            r#"{
                "version": 1,
                "id": "review-coding",
                "kind": "routing",
                "input": { "role": "review", "complexity": "coding" },
                "expectation": { "runner": "claude", "tier": "deep" }
            }"#,
        )
        .expect("write case");

        let suite = load_suite(dir.path()).expect("load suite");
        assert_eq!(suite.config.name, "routing");
        assert!((suite.config.pass_threshold - 1.0).abs() < f64::EPSILON);
        assert_eq!(suite.cases.len(), 1);
        assert!(suite.case("review-coding").is_some());
    }

    #[test]
    fn load_suite_rejects_unknown_version_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("suite.toml"),
            "name = \"routing\"\npass_threshold = 1.0\n",
        )
        .expect("write suite.toml");
        std::fs::write(
            dir.path().join("future.json"),
            r#"{
                "version": 7,
                "id": "x",
                "kind": "routing",
                "input": { "role": "impl", "complexity": "simple" },
                "expectation": { "runner": "local", "tier": "local" }
            }"#,
        )
        .expect("write case");

        let err = load_suite(dir.path()).expect_err("must reject unknown version");
        match err {
            CaseError::UnsupportedVersion { version, .. } => assert_eq!(version, 7),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn loads_toml_case_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("suite.toml"),
            "name = \"routing\"\npass_threshold = 0.5\n",
        )
        .expect("write suite.toml");
        std::fs::write(
            dir.path().join("impl.toml"),
            "version = 1\nid = \"impl-simple\"\nkind = \"routing\"\n\n[input]\nrole = \"impl\"\ncomplexity = \"simple\"\n\n[expectation]\nrunner = \"local\"\ntier = \"local\"\n",
        )
        .expect("write toml case");

        let suite = load_suite(dir.path()).expect("load suite");
        assert_eq!(suite.cases.len(), 1);
        assert_eq!(suite.cases[0].id, "impl-simple");
    }
}
