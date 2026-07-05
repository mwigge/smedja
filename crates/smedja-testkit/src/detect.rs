//! Recursive multi-marker test-suite detection.
//!
//! [`detect_suites`] walks a workspace and returns a *list* of suites — one per
//! language toolchain found — so a mixed-language repo runs each of its test
//! commands. When a monorepo meta-runner is present at the root (nx, turbo,
//! moon, just, task) the scan short-circuits to a single delegating suite: the
//! meta-runner owns fan-out across its projects.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A test runner smedja knows how to detect and drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runner {
    /// Rust — `Cargo.toml`.
    Cargo,
    /// Node — `package.json` with a `scripts.test`.
    Npm,
    /// Python — `pyproject.toml` / `pytest.ini` / `tox.ini` / `setup.cfg`.
    Pytest,
    /// Go — `go.mod`.
    Go,
    /// JVM (Maven) — `pom.xml`.
    Maven,
    /// JVM (Gradle) — `build.gradle` / `build.gradle.kts`.
    Gradle,
    /// .NET — `*.csproj`.
    DotNet,
    /// Nx monorepo meta-runner — `nx.json`.
    Nx,
    /// Turborepo meta-runner — `turbo.json`.
    Turbo,
    /// Moon meta-runner — `.moon/` / `moon.yml`.
    Moon,
    /// Just task runner — `Justfile`.
    Just,
    /// Task (go-task) runner — `Taskfile.yml`.
    Task,
}

impl Runner {
    /// The lowercase wire label used in [`crate::SuiteReport::runner`].
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Npm => "npm",
            Self::Pytest => "pytest",
            Self::Go => "go",
            Self::Maven => "maven",
            Self::Gradle => "gradle",
            Self::DotNet => "dotnet",
            Self::Nx => "nx",
            Self::Turbo => "turbo",
            Self::Moon => "moon",
            Self::Just => "just",
            Self::Task => "task",
        }
    }

    /// `true` for the monorepo meta-runners that fan out to sub-projects.
    #[must_use]
    pub fn is_meta(self) -> bool {
        matches!(
            self,
            Self::Nx | Self::Turbo | Self::Moon | Self::Just | Self::Task
        )
    }
}

/// A detected test suite: a runner rooted at a directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suite {
    /// Which runner drives this suite.
    pub runner: Runner,
    /// The directory the suite is rooted at (contains the marker file).
    pub dir: PathBuf,
}

/// Directory names never worth descending into during detection.
const IGNORED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    "dist",
    "build",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    ".claude",
    ".next",
    ".turbo",
    "bin",
    "obj",
];

/// Maximum directory depth walked during detection.
const MAX_DEPTH: usize = 6;

/// Detects every test suite under `root`.
///
/// A monorepo meta-runner at the root (checked first) wins and yields a single
/// delegating suite. Otherwise the tree is scanned recursively and one suite is
/// returned per language toolchain, keeping only the *shallowest* marker per
/// runner within any subtree (so a Cargo workspace or npm monorepo root is not
/// duplicated by its members).
#[must_use]
pub fn detect_suites(root: &Path) -> Vec<Suite> {
    if let Some(meta) = detect_meta(root) {
        return vec![Suite {
            runner: meta,
            dir: root.to_path_buf(),
        }];
    }

    let mut hits: Vec<Suite> = Vec::new();
    scan(root, 0, &mut hits);
    shallowest_per_runner(hits)
}

/// Detects a root-level monorepo meta-runner, in precedence order.
fn detect_meta(root: &Path) -> Option<Runner> {
    if root.join("nx.json").is_file() {
        return Some(Runner::Nx);
    }
    if root.join("turbo.json").is_file() {
        return Some(Runner::Turbo);
    }
    if root.join("moon.yml").is_file() || root.join(".moon").is_dir() {
        return Some(Runner::Moon);
    }
    if root.join("Justfile").is_file() || root.join("justfile").is_file() {
        return Some(Runner::Just);
    }
    if root.join("Taskfile.yml").is_file() || root.join("Taskfile.yaml").is_file() {
        return Some(Runner::Task);
    }
    None
}

/// Recursively collects per-directory marker hits, bounded by [`MAX_DEPTH`].
fn scan(dir: &Path, depth: usize, out: &mut Vec<Suite>) {
    for runner in markers_in(dir) {
        out.push(Suite {
            runner,
            dir: dir.to_path_buf(),
        });
    }
    if depth >= MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') && name != "." || IGNORED_DIRS.contains(&name.as_ref()) {
            continue;
        }
        scan(&path, depth + 1, out);
    }
}

/// Returns the runners whose marker files are present directly in `dir`.
fn markers_in(dir: &Path) -> Vec<Runner> {
    let mut found = Vec::new();
    if dir.join("Cargo.toml").is_file() {
        found.push(Runner::Cargo);
    }
    if npm_has_test_script(dir) {
        found.push(Runner::Npm);
    }
    if dir.join("pyproject.toml").is_file()
        || dir.join("pytest.ini").is_file()
        || dir.join("tox.ini").is_file()
        || setup_cfg_has_pytest(dir)
    {
        found.push(Runner::Pytest);
    }
    if dir.join("go.mod").is_file() {
        found.push(Runner::Go);
    }
    if dir.join("pom.xml").is_file() {
        found.push(Runner::Maven);
    }
    if dir.join("build.gradle").is_file() || dir.join("build.gradle.kts").is_file() {
        found.push(Runner::Gradle);
    }
    if has_csproj(dir) {
        found.push(Runner::DotNet);
    }
    found
}

/// `true` when `dir/package.json` exists and declares a `scripts.test`.
fn npm_has_test_script(dir: &Path) -> bool {
    let path = dir.join("package.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    json.get("scripts")
        .and_then(|s| s.get("test"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

/// `true` when `dir/setup.cfg` carries a `[tool:pytest]` section.
fn setup_cfg_has_pytest(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("setup.cfg")).is_ok_and(|s| s.contains("[tool:pytest]"))
}

/// `true` when `dir` contains at least one `*.csproj` file.
fn has_csproj(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.path()
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("csproj"))
    })
}

/// Keeps only the shallowest suite per runner within any ancestor chain, so a
/// workspace root is not duplicated by nested members of the same toolchain.
fn shallowest_per_runner(mut hits: Vec<Suite>) -> Vec<Suite> {
    // Sort by (runner, path length) so shallower dirs come first per runner.
    hits.sort_by(|a, b| {
        a.runner
            .cmp(&b.runner)
            .then_with(|| a.dir.as_os_str().len().cmp(&b.dir.as_os_str().len()))
            .then_with(|| a.dir.cmp(&b.dir))
    });
    let mut kept: Vec<Suite> = Vec::new();
    let mut seen: BTreeSet<(Runner, PathBuf)> = BTreeSet::new();
    for hit in hits {
        // Drop when an already-kept suite of the same runner is an ancestor.
        let nested_under_kept = kept
            .iter()
            .any(|k| k.runner == hit.runner && hit.dir.starts_with(&k.dir));
        if nested_under_kept {
            continue;
        }
        if seen.insert((hit.runner, hit.dir.clone())) {
            kept.push(hit);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn detects_mixed_language_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "Cargo.toml", "[package]\nname='x'\n");
        touch(root, "go.mod", "module x\n");
        touch(root, "package.json", r#"{"scripts":{"test":"jest"}}"#);
        touch(
            &root.join("py"),
            "pyproject.toml",
            "[tool.pytest.ini_options]\n",
        );

        let suites = detect_suites(root);
        let runners: BTreeSet<Runner> = suites.iter().map(|s| s.runner).collect();
        assert!(runners.contains(&Runner::Cargo));
        assert!(runners.contains(&Runner::Go));
        assert!(runners.contains(&Runner::Npm));
        assert!(runners.contains(&Runner::Pytest));
    }

    #[test]
    fn package_json_without_test_script_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "package.json", r#"{"scripts":{"build":"tsc"}}"#);
        let suites = detect_suites(tmp.path());
        assert!(suites.iter().all(|s| s.runner != Runner::Npm));
    }

    #[test]
    fn meta_runner_short_circuits_to_single_suite() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "nx.json", "{}");
        touch(tmp.path(), "package.json", r#"{"scripts":{"test":"nx"}}"#);
        touch(tmp.path(), "Cargo.toml", "[package]\nname='x'\n");
        let suites = detect_suites(tmp.path());
        assert_eq!(suites.len(), 1);
        assert_eq!(suites[0].runner, Runner::Nx);
    }

    #[test]
    fn nested_cargo_members_collapse_to_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "Cargo.toml", "[workspace]\n");
        touch(
            &tmp.path().join("crates/a"),
            "Cargo.toml",
            "[package]\nname='a'\n",
        );
        touch(
            &tmp.path().join("crates/b"),
            "Cargo.toml",
            "[package]\nname='b'\n",
        );
        let suites = detect_suites(tmp.path());
        let cargo: Vec<_> = suites
            .iter()
            .filter(|s| s.runner == Runner::Cargo)
            .collect();
        assert_eq!(cargo.len(), 1);
        assert_eq!(cargo[0].dir, tmp.path());
    }

    #[test]
    fn ignored_dirs_are_not_descended() {
        let tmp = tempfile::tempdir().unwrap();
        touch(
            &tmp.path().join("node_modules/pkg"),
            "go.mod",
            "module dep\n",
        );
        let suites = detect_suites(tmp.path());
        assert!(suites.is_empty());
    }
}
