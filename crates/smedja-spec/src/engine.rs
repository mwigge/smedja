//! [`SpecEngine`] — file-backed operations over an `openspec/` tree.
//!
//! The engine owns every read/write of the OpenSpec file model so there is one
//! code path for specs, changes, deltas, validation, and archival:
//!
//! - `openspec/specs/<capability>/spec.md` — the source of truth.
//! - `openspec/changes/<name>/{proposal,design,tasks}.md` — a change's artifacts.
//! - `openspec/changes/<name>/specs/<capability>/spec.md` — a change's delta.
//! - `openspec/changes/archive/<name>/` — where completed changes land.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Delta, Requirement, Spec};
use crate::parse::{parse_delta, parse_spec, render_delta, render_spec, task_counts};

/// The structural-validation report for a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    /// The change that was validated.
    pub change: String,
    /// Whether the change passes: `errors` empty (and, under `strict`, the
    /// strict-only checks folded into `errors` as well).
    pub valid: bool,
    /// Whether the report was produced under `--strict`.
    pub strict: bool,
    /// Hard failures that always block.
    pub errors: Vec<String>,
    /// Advisory findings that never block on their own.
    pub warnings: Vec<String>,
}

/// The outcome of archiving a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveOutcome {
    /// The archived change name.
    pub change: String,
    /// The capabilities whose specs were updated by the merge.
    pub capabilities: Vec<String>,
    /// Where the change directory was moved to.
    pub archived_path: PathBuf,
}

/// A change's at-a-glance status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeStatus {
    /// The change name.
    pub name: String,
    /// Whether `proposal.md` exists.
    pub has_proposal: bool,
    /// Whether `design.md` exists.
    pub has_design: bool,
    /// Whether `tasks.md` exists.
    pub has_tasks: bool,
    /// The capabilities the change carries deltas for.
    pub delta_capabilities: Vec<String>,
    /// Total task items in `tasks.md`.
    pub tasks_total: usize,
    /// Completed task items in `tasks.md`.
    pub tasks_done: usize,
    /// Whether the change passes non-strict validation.
    pub valid: bool,
}

/// A recoverable engine error.
#[derive(Debug)]
pub enum SpecError {
    /// A change/capability name contained a path separator or `..`.
    UnsafeName(String),
    /// The referenced change directory does not exist.
    ChangeMissing(String),
    /// A change already exists at create time.
    ChangeExists(String),
    /// An archive destination already exists.
    ArchiveExists(String),
    /// An underlying filesystem error.
    Io(String),
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpecError::UnsafeName(n) => write!(f, "unsafe name: {n}"),
            SpecError::ChangeMissing(n) => write!(f, "change not found: {n}"),
            SpecError::ChangeExists(n) => write!(f, "change already exists: {n}"),
            SpecError::ArchiveExists(n) => write!(f, "archive already exists: {n}"),
            SpecError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for SpecError {}

impl From<std::io::Error> for SpecError {
    fn from(e: std::io::Error) -> Self {
        SpecError::Io(e.to_string())
    }
}

/// Result alias for engine operations.
pub type Result<T> = std::result::Result<T, SpecError>;

/// Rejects a name that could escape its directory (path separators or `..`).
fn safe_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name == "archive"
    {
        return Err(SpecError::UnsafeName(name.to_owned()));
    }
    Ok(())
}

/// A file-backed OpenSpec engine rooted at an `openspec/` directory.
pub struct SpecEngine {
    root: PathBuf,
}

impl SpecEngine {
    /// Creates an engine rooted at an existing (or to-be-created) `openspec/`
    /// directory.
    pub fn new(openspec_root: impl Into<PathBuf>) -> Self {
        Self {
            root: openspec_root.into(),
        }
    }

    /// Creates an engine for `<workspace_root>/openspec`.
    #[must_use]
    pub fn at_workspace(workspace_root: &Path) -> Self {
        Self::new(workspace_root.join("openspec"))
    }

    /// The `openspec/` root this engine operates on.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn specs_dir(&self) -> PathBuf {
        self.root.join("specs")
    }

    fn changes_dir(&self) -> PathBuf {
        self.root.join("changes")
    }

    fn change_dir(&self, name: &str) -> PathBuf {
        self.changes_dir().join(name)
    }

    fn archive_dir(&self) -> PathBuf {
        self.changes_dir().join("archive")
    }

    fn spec_path(&self, capability: &str) -> PathBuf {
        self.specs_dir().join(capability).join("spec.md")
    }

    fn delta_dir(&self, change: &str) -> PathBuf {
        self.change_dir(change).join("specs")
    }

    fn delta_path(&self, change: &str, capability: &str) -> PathBuf {
        self.delta_dir(change).join(capability).join("spec.md")
    }

    // ── changes ──────────────────────────────────────────────────────────────

    /// Scaffolds a new change directory with `proposal.md`, `design.md`, and
    /// `tasks.md`, returning the files written.
    ///
    /// # Errors
    ///
    /// Returns [`SpecError`] if `name` is unsafe, the change already exists, or
    /// a file write fails.
    pub fn create_change(&self, name: &str, why: &str, what: &str) -> Result<Vec<PathBuf>> {
        safe_name(name)?;
        let dir = self.change_dir(name);
        if dir.exists() {
            return Err(SpecError::ChangeExists(name.to_owned()));
        }
        std::fs::create_dir_all(&dir)?;

        let why = if why.trim().is_empty() {
            "_Describe the problem this change solves._"
        } else {
            why.trim()
        };
        let what = if what.trim().is_empty() {
            "_Describe what changes._"
        } else {
            what.trim()
        };
        let proposal = format!("# Change: {name}\n\n## Why\n{why}\n\n## What Changes\n{what}\n");
        let design = format!("# Design: {name}\n\n## Context\n\n## Decisions\n");
        let tasks = "## 1. Implementation\n\n- [ ] 1.1 Describe the first slice\n".to_owned();

        let files = [
            (dir.join("proposal.md"), proposal),
            (dir.join("design.md"), design),
            (dir.join("tasks.md"), tasks),
        ];
        let mut written = Vec::with_capacity(files.len());
        for (path, content) in files {
            std::fs::write(&path, content)?;
            written.push(path);
        }
        Ok(written)
    }

    /// Returns whether a change directory exists.
    #[must_use]
    pub fn change_exists(&self, name: &str) -> bool {
        safe_name(name).is_ok() && self.change_dir(name).is_dir()
    }

    /// Lists active (non-archived) change names, sorted.
    #[must_use]
    pub fn list_changes(&self) -> Vec<String> {
        list_dir_names(&self.changes_dir())
            .into_iter()
            .filter(|n| n != "archive")
            .collect()
    }

    /// Lists archived change names, sorted.
    #[must_use]
    pub fn list_archived(&self) -> Vec<String> {
        list_dir_names(&self.archive_dir())
    }

    /// Lists known capability spec names, sorted.
    #[must_use]
    pub fn list_specs(&self) -> Vec<String> {
        list_dir_names(&self.specs_dir())
    }

    // ── specs & deltas ───────────────────────────────────────────────────────

    /// Reads a capability's source spec, or `None` if it does not exist.
    #[must_use]
    pub fn read_spec(&self, capability: &str) -> Option<Spec> {
        let md = std::fs::read_to_string(self.spec_path(capability)).ok()?;
        Some(parse_spec(capability, &md))
    }

    /// Writes a capability's source spec.
    ///
    /// # Errors
    ///
    /// Returns [`SpecError`] on an unsafe capability name or a write failure.
    pub fn write_spec(&self, spec: &Spec) -> Result<PathBuf> {
        safe_name(&spec.capability)?;
        let path = self.spec_path(&spec.capability);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, render_spec(spec))?;
        Ok(path)
    }

    /// Writes a change's delta spec for `capability` from raw markdown.
    ///
    /// # Errors
    ///
    /// Returns [`SpecError`] on an unsafe name, a missing change, or a write
    /// failure.
    pub fn write_delta(&self, change: &str, capability: &str, delta_md: &str) -> Result<PathBuf> {
        safe_name(change)?;
        safe_name(capability)?;
        if !self.change_dir(change).is_dir() {
            return Err(SpecError::ChangeMissing(change.to_owned()));
        }
        let path = self.delta_path(change, capability);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, delta_md)?;
        Ok(path)
    }

    /// Reads and parses a change's delta for `capability`, or `None` if absent.
    #[must_use]
    pub fn read_delta(&self, change: &str, capability: &str) -> Option<Delta> {
        let md = std::fs::read_to_string(self.delta_path(change, capability)).ok()?;
        Some(parse_delta(capability, &md))
    }

    /// Lists the capabilities a change carries deltas for, sorted.
    #[must_use]
    pub fn list_delta_capabilities(&self, change: &str) -> Vec<String> {
        list_dir_names(&self.delta_dir(change))
    }

    /// Reads and parses every delta a change carries.
    #[must_use]
    pub fn read_deltas(&self, change: &str) -> Vec<Delta> {
        self.list_delta_capabilities(change)
            .into_iter()
            .filter_map(|cap| self.read_delta(change, &cap))
            .collect()
    }

    // ── tasks ────────────────────────────────────────────────────────────────

    /// Reads a change's `tasks.md`, or an empty string if absent.
    #[must_use]
    pub fn read_tasks(&self, change: &str) -> String {
        std::fs::read_to_string(self.change_dir(change).join("tasks.md")).unwrap_or_default()
    }

    // ── validation ───────────────────────────────────────────────────────────

    /// Validates a change structurally.
    ///
    /// Hard errors (always block): every `ADDED`/`MODIFIED` requirement has at
    /// least one scenario; every `MODIFIED`/`REMOVED` requirement references a
    /// real capability spec and a real requirement within it. Under `strict`,
    /// two more checks fold into the errors: every `ADDED`/`MODIFIED`
    /// requirement asserts `SHALL`/`MUST`, and `tasks.md` has at least one task
    /// item. Missing `proposal.md`/deltas are advisory warnings.
    #[must_use]
    pub fn validate(&self, change: &str, strict: bool) -> ValidationReport {
        let mut errors: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        if safe_name(change).is_err() {
            errors.push(format!("invalid change name: {change}"));
        }
        if !self.change_dir(change).is_dir() {
            errors.push(format!("change directory not found: {change}"));
            return ValidationReport {
                change: change.to_owned(),
                valid: false,
                strict,
                errors,
                warnings,
            };
        }

        if !self.change_dir(change).join("proposal.md").is_file() {
            warnings.push("proposal.md is missing".to_owned());
        }

        let deltas = self.read_deltas(change);
        if deltas.is_empty() {
            warnings.push("change has no delta specs".to_owned());
        }

        for delta in &deltas {
            let cap = &delta.capability;
            let source = self.read_spec(cap);

            for req in delta.added.iter().chain(&delta.modified) {
                if req.scenarios.is_empty() {
                    errors.push(format!("{cap}: requirement '{}' has no scenario", req.name));
                }
                if strict && !req.is_normative() {
                    errors.push(format!(
                        "{cap}: requirement '{}' does not assert SHALL/MUST",
                        req.name
                    ));
                }
            }

            for req in delta.modified.iter().chain(&delta.removed) {
                match &source {
                    None => errors.push(format!(
                        "{cap}: delta references capability with no source spec"
                    )),
                    Some(spec) if !spec.has_requirement(&req.name) => errors.push(format!(
                        "{cap}: delta references unknown requirement '{}'",
                        req.name
                    )),
                    Some(_) => {}
                }
            }
        }

        let tasks = self.read_tasks(change);
        let (_, total) = task_counts(&tasks);
        if total == 0 {
            if strict {
                errors.push("tasks.md has no task items".to_owned());
            } else {
                warnings.push("tasks.md has no task items".to_owned());
            }
        }

        ValidationReport {
            change: change.to_owned(),
            valid: errors.is_empty(),
            strict,
            errors,
            warnings,
        }
    }

    // ── status / show / diff ─────────────────────────────────────────────────

    /// Returns a change's at-a-glance status.
    #[must_use]
    pub fn status(&self, change: &str) -> ChangeStatus {
        let dir = self.change_dir(change);
        let tasks = self.read_tasks(change);
        let (done, total) = task_counts(&tasks);
        ChangeStatus {
            name: change.to_owned(),
            has_proposal: dir.join("proposal.md").is_file(),
            has_design: dir.join("design.md").is_file(),
            has_tasks: dir.join("tasks.md").is_file(),
            delta_capabilities: self.list_delta_capabilities(change),
            tasks_total: total,
            tasks_done: done,
            valid: self.validate(change, false).valid,
        }
    }

    /// Renders a human-readable summary of a change (proposal + deltas +
    /// validation).
    #[must_use]
    pub fn show(&self, change: &str) -> String {
        let dir = self.change_dir(change);
        let mut out = format!("# Change: {change}\n\n");

        let proposal = std::fs::read_to_string(dir.join("proposal.md")).unwrap_or_default();
        if proposal.trim().is_empty() {
            out.push_str("(no proposal.md)\n\n");
        } else {
            out.push_str(proposal.trim());
            out.push_str("\n\n");
        }

        let caps = self.list_delta_capabilities(change);
        if caps.is_empty() {
            out.push_str("Deltas: (none)\n");
        } else {
            out.push_str(&format!("Deltas: {}\n", caps.join(", ")));
        }

        let report = self.validate(change, false);
        out.push_str(&format!(
            "\nValidation: {}\n",
            if report.valid { "pass" } else { "fail" }
        ));
        for e in &report.errors {
            out.push_str(&format!("  error: {e}\n"));
        }
        for w in &report.warnings {
            out.push_str(&format!("  warn: {w}\n"));
        }
        out
    }

    /// Renders every delta the change carries as markdown, capability by
    /// capability.
    #[must_use]
    pub fn diff(&self, change: &str) -> String {
        let mut out = String::new();
        for delta in self.read_deltas(change) {
            out.push_str(&format!("## spec: {}\n\n", delta.capability));
            out.push_str(&render_delta(&delta));
            out.push('\n');
        }
        if out.is_empty() {
            out.push_str("(no deltas)\n");
        }
        out
    }

    // ── archive ──────────────────────────────────────────────────────────────

    /// Merges a change's deltas into the source specs, then moves the change to
    /// `changes/archive/<name>`.
    ///
    /// The merge is: `ADDED` requirements are appended (replacing a same-named
    /// one to stay idempotent), `MODIFIED` replace the same-named requirement,
    /// `REMOVED` delete it.
    ///
    /// # Errors
    ///
    /// Returns [`SpecError`] on an unsafe name, a missing change, an existing
    /// archive destination, or a filesystem failure.
    pub fn archive(&self, change: &str) -> Result<ArchiveOutcome> {
        safe_name(change)?;
        let dir = self.change_dir(change);
        if !dir.is_dir() {
            return Err(SpecError::ChangeMissing(change.to_owned()));
        }

        let mut touched: Vec<String> = Vec::new();
        for delta in self.read_deltas(change) {
            let mut spec = self
                .read_spec(&delta.capability)
                .unwrap_or_else(|| Spec::new_empty(&delta.capability));
            merge_delta_into_spec(&mut spec, &delta);
            self.write_spec(&spec)?;
            touched.push(delta.capability.clone());
        }

        std::fs::create_dir_all(self.archive_dir())?;
        let dest = self.archive_dir().join(change);
        if dest.exists() {
            return Err(SpecError::ArchiveExists(change.to_owned()));
        }
        std::fs::rename(&dir, &dest)?;

        Ok(ArchiveOutcome {
            change: change.to_owned(),
            capabilities: touched,
            archived_path: dest,
        })
    }
}

/// Applies a delta to a spec in place: `ADDED` appended (dedup by name),
/// `MODIFIED` replaces by name, `REMOVED` deletes by name.
fn merge_delta_into_spec(spec: &mut Spec, delta: &Delta) {
    for req in &delta.added {
        upsert_requirement(spec, req.clone());
    }
    for req in &delta.modified {
        upsert_requirement(spec, req.clone());
    }
    for req in &delta.removed {
        spec.requirements.retain(|r| r.name != req.name);
    }
}

/// Replaces the same-named requirement in place, or appends it.
fn upsert_requirement(spec: &mut Spec, req: Requirement) {
    if let Some(existing) = spec.requirement_mut(&req.name) {
        *existing = req;
    } else {
        spec.requirements.push(req);
    }
}

/// Returns the sorted immediate sub-directory names of `dir`, or an empty vec.
fn list_dir_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> (tempfile::TempDir, SpecEngine) {
        let dir = tempfile::tempdir().unwrap();
        let engine = SpecEngine::at_workspace(dir.path());
        (dir, engine)
    }

    #[test]
    fn create_change_scaffolds_the_three_artifacts() {
        let (_tmp, eng) = engine();
        let written = eng
            .create_change("add-widget", "the why", "the what")
            .unwrap();
        assert_eq!(written.len(), 3);
        assert!(eng.change_exists("add-widget"));
        assert!(eng.status("add-widget").has_proposal);
        assert!(eng.status("add-widget").has_tasks);
        // A second create refuses.
        assert!(eng.create_change("add-widget", "", "").is_err());
    }

    #[test]
    fn create_change_rejects_unsafe_name() {
        let (_tmp, eng) = engine();
        assert!(eng.create_change("../escape", "", "").is_err());
        assert!(eng.create_change("a/b", "", "").is_err());
    }

    #[test]
    fn validate_flags_requirement_without_scenario() {
        let (_tmp, eng) = engine();
        eng.create_change("c", "w", "t").unwrap();
        // A delta whose ADDED requirement has no scenario.
        eng.write_delta(
            "c",
            "widget",
            "## ADDED Requirements\n\n### Requirement: No Scenario\nThe system SHALL do things.\n",
        )
        .unwrap();
        let report = eng.validate("c", false);
        assert!(!report.valid);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("No Scenario") && e.contains("no scenario")),
            "expected a no-scenario error; got {:?}",
            report.errors
        );
    }

    #[test]
    fn validate_passes_a_well_formed_change() {
        let (_tmp, eng) = engine();
        eng.create_change("c", "w", "t").unwrap();
        eng.write_delta(
            "c",
            "widget",
            "## ADDED Requirements\n\n### Requirement: Works\nThe system SHALL work.\n\n#### Scenario: Basic\n- WHEN used\n- THEN it works\n",
        )
        .unwrap();
        let report = eng.validate("c", true);
        assert!(report.valid, "expected valid; got {:?}", report.errors);
    }

    #[test]
    fn validate_strict_flags_missing_shall_and_empty_tasks() {
        let (_tmp, eng) = engine();
        eng.create_change("c", "w", "t").unwrap();
        // Requirement without SHALL/MUST, and blank tasks.
        eng.write_delta(
            "c",
            "widget",
            "## ADDED Requirements\n\n### Requirement: Weak\nThe system does things.\n\n#### Scenario: S\n- THEN ok\n",
        )
        .unwrap();
        std::fs::write(eng.change_dir("c").join("tasks.md"), "no tasks here\n").unwrap();
        let strict = eng.validate("c", true);
        assert!(!strict.valid);
        assert!(strict.errors.iter().any(|e| e.contains("SHALL/MUST")));
        assert!(strict.errors.iter().any(|e| e.contains("no task items")));
        // Non-strict: SHALL-less requirement and empty tasks are not hard errors.
        let lax = eng.validate("c", false);
        assert!(
            lax.valid,
            "non-strict must not block on those; {:?}",
            lax.errors
        );
    }

    #[test]
    fn validate_flags_modify_of_unknown_capability() {
        let (_tmp, eng) = engine();
        eng.create_change("c", "w", "t").unwrap();
        eng.write_delta(
            "c",
            "ghost",
            "## MODIFIED Requirements\n\n### Requirement: Nope\nThe system SHALL x.\n\n#### Scenario: S\n- THEN ok\n",
        )
        .unwrap();
        let report = eng.validate("c", false);
        assert!(!report.valid);
        assert!(report.errors.iter().any(|e| e.contains("no source spec")));
    }

    #[test]
    fn archive_merges_added_modified_removed_then_moves_change() {
        let (_tmp, eng) = engine();
        // Seed a source spec with two requirements.
        let mut seed = Spec::new_empty("widget");
        seed.requirements.push({
            let mut r = Requirement::new("Keepme", "The system SHALL keep.");
            r.scenarios
                .push(crate::model::Scenario::new("S", "- THEN kept"));
            r
        });
        seed.requirements.push({
            let mut r = Requirement::new("Changeme", "The system SHALL be old.");
            r.scenarios
                .push(crate::model::Scenario::new("S", "- THEN old"));
            r
        });
        seed.requirements.push({
            let mut r = Requirement::new("Dropme", "The system SHALL vanish.");
            r.scenarios
                .push(crate::model::Scenario::new("S", "- THEN gone"));
            r
        });
        eng.write_spec(&seed).unwrap();

        eng.create_change("c", "w", "t").unwrap();
        eng.write_delta(
            "c",
            "widget",
            "## ADDED Requirements\n\n### Requirement: Newone\nThe system SHALL be new.\n\n#### Scenario: S\n- THEN new\n\n\
             ## MODIFIED Requirements\n\n### Requirement: Changeme\nThe system SHALL be updated.\n\n#### Scenario: S\n- THEN updated\n\n\
             ## REMOVED Requirements\n\n### Requirement: Dropme\n",
        )
        .unwrap();

        let outcome = eng.archive("c").unwrap();
        assert_eq!(outcome.capabilities, vec!["widget".to_owned()]);

        let merged = eng.read_spec("widget").unwrap();
        let names: Vec<&str> = merged
            .requirements
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert!(names.contains(&"Keepme"), "ADDED must not disturb existing");
        assert!(names.contains(&"Newone"), "ADDED requirement appended");
        assert!(
            names.contains(&"Changeme"),
            "MODIFIED requirement kept by name"
        );
        assert!(!names.contains(&"Dropme"), "REMOVED requirement deleted");
        let changed = merged
            .requirements
            .iter()
            .find(|r| r.name == "Changeme")
            .unwrap();
        assert!(
            changed.text.contains("updated"),
            "MODIFIED must replace the definition"
        );

        // The change directory has moved into the archive.
        assert!(!eng.change_exists("c"));
        assert_eq!(eng.list_archived(), vec!["c".to_owned()]);
        assert!(outcome.archived_path.is_dir());
    }

    #[test]
    fn list_changes_excludes_archive() {
        let (_tmp, eng) = engine();
        eng.create_change("alpha", "", "").unwrap();
        eng.create_change("beta", "", "").unwrap();
        std::fs::create_dir_all(eng.archive_dir().join("old")).unwrap();
        assert_eq!(
            eng.list_changes(),
            vec!["alpha".to_owned(), "beta".to_owned()]
        );
    }

    #[test]
    fn diff_renders_the_deltas() {
        let (_tmp, eng) = engine();
        eng.create_change("c", "", "").unwrap();
        eng.write_delta(
            "c",
            "widget",
            "## ADDED Requirements\n\n### Requirement: R\nThe system SHALL x.\n\n#### Scenario: S\n- THEN ok\n",
        )
        .unwrap();
        let diff = eng.diff("c");
        assert!(diff.contains("## spec: widget"));
        assert!(diff.contains("ADDED Requirements"));
        assert!(diff.contains("Requirement: R"));
    }
}
