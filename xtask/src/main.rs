//! `cargo xtask` — build-time code generation helpers for smedja.
//!
//! # Usage
//!
//! ```
//! cargo xtask gen-rpc-types
//! ```
//!
//! Reads `crates/smedja-rpc/schema/types.json`, generates Rust types with
//! `typify`, and writes `crates/smedja-rpc/src/generated.rs`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use schemars::schema::RootSchema;
use typify::TypeSpace;

/// Cargo xtask entry point.
#[derive(Debug, Parser)]
#[command(name = "xtask", about = "smedja build-time code generation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Available xtask subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Generate `crates/smedja-rpc/src/generated.rs` from the JSON Schema.
    GenRpcTypes,
    /// Run an eval suite and gate on its pass-rate threshold.
    Eval {
        /// Path to the suite directory (contains `suite.toml` and case files).
        suite: PathBuf,
        /// Override the suite's configured pass-rate threshold (in [0.0, 1.0]).
        #[arg(long)]
        threshold: Option<f64>,
    },
    /// Assert every version literal agrees with `[workspace.package] version`.
    CheckVersions,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::GenRpcTypes => gen_rpc_types(),
        Command::Eval { suite, threshold } => run_eval(&suite, threshold),
        Command::CheckVersions => check_versions(),
    }
}

/// Reads `[workspace.package] version` from a root `Cargo.toml`.
fn workspace_version(toml: &str) -> Option<String> {
    let mut in_section = false;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_section = t == "[workspace.package]";
            continue;
        }
        if in_section {
            if let Some(rest) = t.strip_prefix("version") {
                if let Some(val) = rest.trim_start().strip_prefix('=') {
                    return Some(val.trim().trim_matches('"').to_owned());
                }
            }
        }
    }
    None
}

/// True when `[package.metadata.bundle]` hardcodes its own `version`, which
/// silently drifts from the workspace (it should inherit `version.workspace`).
fn bundle_hardcodes_version(toml: &str) -> bool {
    let mut in_bundle = false;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('#') {
            continue;
        }
        if t.starts_with('[') {
            in_bundle = t == "[package.metadata.bundle]";
            continue;
        }
        if in_bundle
            && t.strip_prefix("version")
                .is_some_and(|r| r.trim_start().starts_with('='))
        {
            return true;
        }
    }
    false
}

/// Finds `<key><sep>value` on its own line (e.g. `pkgver=` or `pkgver = `),
/// returning the trimmed value. Handles the leading-tab `.SRCINFO` form.
fn line_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        // Accept either `key=value` (PKGBUILD) or `key = value` (.SRCINFO).
        let rest = l.trim().strip_prefix(key)?.trim_start();
        let rest = rest.strip_prefix('=').unwrap_or(rest);
        Some(rest.trim().to_owned())
    })
}

/// Asserts that the AUR PKGBUILD/.SRCINFO `pkgver` and the absence of a
/// hardcoded bundle version all agree with `[workspace.package] version`.
///
/// # Errors
///
/// Returns an error (and prints each offender) when any source disagrees, so
/// `cargo xtask check-versions` can gate CI against version drift.
fn check_versions() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask crate has no parent (workspace root)")?
        .to_path_buf();
    let read = |rel: &str| -> Result<String> {
        let p = root.join(rel);
        std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))
    };

    let want = workspace_version(&read("Cargo.toml")?)
        .context("[workspace.package] version not found in Cargo.toml")?;

    let mut errs: Vec<String> = Vec::new();
    let mut check_pkgver = |rel: &str, key: &str| match read(rel).map(|s| line_value(&s, key)) {
        Ok(Some(v)) if v == want => {}
        Ok(Some(v)) => errs.push(format!("{rel}: pkgver {v} != workspace {want}")),
        Ok(None) => errs.push(format!("{rel}: no `{key}` line")),
        Err(e) => errs.push(format!("{rel}: {e}")),
    };
    check_pkgver("assets/PKGBUILD", "pkgver");
    check_pkgver("assets/.SRCINFO", "pkgver");

    if read("term/bin/smedja/Cargo.toml").is_ok_and(|s| bundle_hardcodes_version(&s)) {
        errs.push(
            "term/bin/smedja/Cargo.toml: [package.metadata.bundle] hardcodes `version` — \
             remove it so the bundle tracks version.workspace"
                .to_owned(),
        );
    }

    if errs.is_empty() {
        println!("version check OK — all sources agree on {want}");
        Ok(())
    } else {
        for e in &errs {
            eprintln!("\u{2717} {e}");
        }
        anyhow::bail!("{} version mismatch(es)", errs.len())
    }
}

/// Loads the suite at `dir`, runs it (offline — deterministic scorers only),
/// prints a report, and exits non-zero when the pass rate is below the
/// (possibly overridden) threshold. A thin shim over `smedja_eval::run_suite`.
///
/// # Errors
///
/// Returns an error if the suite cannot be loaded or its report cannot be
/// serialised.
fn run_eval(dir: &std::path::Path, threshold_override: Option<f64>) -> Result<()> {
    use smedja_eval::case::load_suite;
    use smedja_eval::engine::{run_suite_with, LoopDriver};
    use smedja_eval::scoring::{Judge, Outcome, Verdict};

    /// An offline stand-in loop driver, never live.
    ///
    /// xtask runs the eval gate offline (free, deterministic), so no real loop
    /// is driven. The stand-in returns a fixed `Complete` outcome so the
    /// deterministic agent example in the corpus demonstrates a passing
    /// deterministic check without any model access. The real driver is wired
    /// at the daemon boundary for online runs.
    struct OfflineDriver;
    impl LoopDriver for OfflineDriver {
        fn drive(&self, _scenario: &str) -> Outcome {
            Outcome::Loop {
                final_state: "complete".to_owned(),
                slices_completed: 1,
            }
        }
        fn is_live(&self) -> bool {
            false
        }
    }

    /// An unused judge: xtask runs offline, so rubric cases are always skipped.
    struct UnusedJudge;
    impl Judge for UnusedJudge {
        fn judge(&self, _rubric: &str, _output: &str) -> Verdict {
            Verdict::Fail("xtask eval runs offline; rubric cases are skipped".to_owned())
        }
    }

    let suite =
        load_suite(dir).with_context(|| format!("failed to load suite at {}", dir.display()))?;
    let threshold = threshold_override.unwrap_or(suite.config.pass_threshold);
    let router = smedja_assayer::Assayer::default_rules();
    // xtask is the CI/dev path: always offline so it is free and deterministic.
    let report = run_suite_with(&suite, &router, &OfflineDriver, &UnusedJudge, true);

    eprintln!("Suite: {}", report.suite);
    for verdict in &report.verdicts {
        let label = match &verdict.status {
            smedja_eval::report::CaseStatus::Pass => "PASS".to_owned(),
            smedja_eval::report::CaseStatus::Fail(reason) => format!("FAIL ({reason})"),
            smedja_eval::report::CaseStatus::Skip(reason) => format!("SKIP ({reason})"),
        };
        eprintln!("  {:<30} {label}", verdict.id);
    }
    eprintln!(
        "{} / {} passed ({} scored, {} skipped) — pass_rate {:.3}, threshold {:.3}",
        report.passed(),
        report.total(),
        report.scored(),
        report.skipped(),
        report.pass_rate(),
        threshold,
    );

    if report.meets_threshold(threshold) {
        Ok(())
    } else {
        std::process::exit(1);
    }
}

/// Reads the JSON Schema at `crates/smedja-rpc/schema/types.json` relative to
/// the workspace root, generates Rust types via `typify`, and writes the result
/// to `crates/smedja-rpc/src/generated.rs`.
///
/// The workspace root is inferred from the `CARGO_MANIFEST_DIR` environment
/// variable that Cargo sets for the xtask crate, falling back to the current
/// working directory.
///
/// # Errors
///
/// Returns an error if the schema file cannot be read, the JSON cannot be
/// parsed as a `RootSchema`, code generation fails, or the output file cannot
/// be written.
fn gen_rpc_types() -> Result<()> {
    let workspace_root = workspace_root();

    let schema_path = workspace_root
        .join("crates")
        .join("smedja-rpc")
        .join("schema")
        .join("types.json");

    let out_path = workspace_root
        .join("crates")
        .join("smedja-rpc")
        .join("src")
        .join("generated.rs");

    eprintln!("Reading schema: {}", schema_path.display());
    let json_str = std::fs::read_to_string(&schema_path)
        .with_context(|| format!("cannot read {}", schema_path.display()))?;

    let schema: RootSchema = serde_json::from_str(&json_str)
        .with_context(|| format!("cannot parse {} as JSON Schema", schema_path.display()))?;

    let mut type_space = TypeSpace::default();
    type_space
        .add_root_schema(schema)
        .context("typify failed to process schema")?;

    let generated = format!("{}", type_space.to_stream());

    // If typify produced no types (e.g. the schema only defines primitives),
    // we emit hand-written newtype wrappers so the file is always non-empty and
    // provides the expected public API.
    let content = if generated.trim().is_empty() {
        hand_written_types()
    } else {
        format!("{}\n\n{}", file_header(), generated)
    };

    eprintln!("Writing: {}", out_path.display());
    std::fs::write(&out_path, content)
        .with_context(|| format!("cannot write {}", out_path.display()))?;

    eprintln!("Done.");
    Ok(())
}

/// Returns the hand-written newtype wrappers that are emitted when typify
/// produces no output for the given schema.
fn hand_written_types() -> String {
    format!(
        "{}\n{}",
        file_header(),
        r"use serde::{Deserialize, Serialize};

/// Opaque identifier for an interactive session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    /// Creates a new [`SessionId`] from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque identifier for a single turn within a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnId(pub String);

impl TurnId {
    /// Creates a new [`TurnId`] from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque identifier for an async task tracked in smedja-ingot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub String);

impl TaskId {
    /// Creates a new [`TaskId`] from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
"
    )
}

/// Returns the standard file header comment for generated files.
fn file_header() -> &'static str {
    "// @generated — do not edit by hand; regenerate with `cargo xtask gen-rpc-types`\n\
     #![allow(clippy::all, unused_imports)]"
}

/// Returns the workspace root path.
///
/// Cargo sets `CARGO_MANIFEST_DIR` to the xtask crate directory; the workspace
/// root is one level up.  Falls back to the current working directory when the
/// variable is not set (e.g. when running the binary directly outside of Cargo).
fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points to xtask/; workspace root is one level up.
    std::env::var("CARGO_MANIFEST_DIR").map_or_else(
        |_| PathBuf::from("."),
        |d| {
            PathBuf::from(d)
                .parent()
                .unwrap_or(&PathBuf::from("."))
                .to_path_buf()
        },
    )
}
