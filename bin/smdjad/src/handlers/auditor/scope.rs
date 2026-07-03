//! Audit scope selection and seed-context building.
//!
//! Resolves the RPC params into an [`AuditScope`] and builds the seed context
//! string the read-only loop starts from. Diff/branch/PR scopes seed from a
//! unified `git diff`; path/repo scopes seed from a `graph_query` symbol listing
//! plus a `list_files` tree.

use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};
use smedja_ingot::IngotHandle;
use smedja_rpc::{codes, RpcError};
use smedja_vault::Vault;
use tokio::sync::Mutex;

use crate::executor::execute_tool;

/// The scope an audit run covers. Each scope maps to a seed-context strategy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuditScope {
    /// Working-tree diff against `HEAD` (`git diff HEAD`).
    Diff,
    /// A path or the whole repository, seeded from graph symbols + a file tree.
    Path { root: String },
    /// A branch range (`git diff <base>...<head>`).
    Branch { base: String, head: String },
    /// A pull request, resolved to a branch range before seeding.
    Pr { reference: String },
}

/// Parses the RPC params into an [`AuditScope`].
///
/// Precedence: `--pr` → `Pr`, `--branch` → `Branch` (head defaults to `HEAD`),
/// an explicit `--diff` flag or no path → `Diff`, a non-empty `path` → `Path`.
#[must_use]
pub(crate) fn resolve_scope(params: &Value) -> AuditScope {
    if let Some(pr) = params.get("pr").and_then(Value::as_str) {
        if !pr.is_empty() {
            return AuditScope::Pr {
                reference: pr.to_owned(),
            };
        }
    }
    if let Some(base) = params.get("branch").and_then(Value::as_str) {
        if !base.is_empty() {
            let head = params
                .get("head")
                .and_then(Value::as_str)
                .filter(|h| !h.is_empty())
                .unwrap_or("HEAD")
                .to_owned();
            return AuditScope::Branch {
                base: base.to_owned(),
                head,
            };
        }
    }
    let diff_requested = params.get("diff").and_then(Value::as_bool).unwrap_or(false);
    if !diff_requested {
        if let Some(path) = params.get("path").and_then(Value::as_str) {
            if !path.is_empty() {
                return AuditScope::Path {
                    root: path.to_owned(),
                };
            }
        }
    }
    AuditScope::Diff
}

/// Runs `git` with `args` in `workspace`, returning stdout on success.
///
/// Uses the async `tokio::process` API so the daemon's runtime is never blocked.
///
/// # Errors
///
/// Returns an [`RpcError`] when the process fails to spawn or exits non-zero.
async fn run_git(workspace: &Path, args: &[&str]) -> Result<String, RpcError> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .await
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("git spawn failed: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RpcError::new(
            codes::INTERNAL_ERROR,
            format!("git {} failed: {stderr}", args.join(" ")),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Resolves a pull-request reference to a `(base, head)` branch range.
///
/// v1 resolves `<base>..<head>` and `<base>...<head>` forms, plus a bare branch
/// name (audited against the repository default `HEAD`).
///
/// # Errors
///
/// Returns an [`RpcError`] when the reference is empty or unparseable.
fn resolve_pr_ref(reference: &str) -> Result<(String, String), RpcError> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err(RpcError::new(
            codes::INVALID_PARAMS,
            "pull-request reference is empty",
        ));
    }
    if let Some((base, head)) = reference.split_once("...") {
        if !base.is_empty() && !head.is_empty() {
            return Ok((base.to_owned(), head.to_owned()));
        }
    }
    if let Some((base, head)) = reference.split_once("..") {
        if !base.is_empty() && !head.is_empty() {
            return Ok((base.to_owned(), head.to_owned()));
        }
    }
    // A bare branch name audits that branch against the merge base with HEAD.
    if !reference.contains(char::is_whitespace) {
        return Ok((reference.to_owned(), "HEAD".to_owned()));
    }
    Err(RpcError::new(
        codes::INVALID_PARAMS,
        format!("cannot resolve pull-request reference: {reference}"),
    ))
}

/// Builds the seed context string for `scope` against `workspace`.
///
/// Diff/branch/PR scopes seed from a unified diff; path/repo scopes seed from a
/// `graph_query` symbol listing plus a `list_files` tree.
///
/// # Errors
///
/// Returns an [`RpcError`] when a `git` invocation fails or a pull-request
/// reference cannot be resolved.
pub(crate) async fn build_seed(
    scope: &AuditScope,
    workspace: &Path,
    ingot: &IngotHandle,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn crate::embedder_port::Embedder>,
) -> Result<String, RpcError> {
    match scope {
        AuditScope::Diff => {
            let diff = run_git(workspace, &["diff", "HEAD"]).await?;
            Ok(format!("Working-tree diff (git diff HEAD):\n\n{diff}"))
        }
        AuditScope::Branch { base, head } => {
            let range = format!("{base}...{head}");
            let diff = run_git(workspace, &["diff", &range]).await?;
            Ok(format!("Branch-range diff (git diff {range}):\n\n{diff}"))
        }
        AuditScope::Pr { reference } => {
            let (base, head) = resolve_pr_ref(reference)?;
            let range = format!("{base}...{head}");
            let diff = run_git(workspace, &["diff", &range]).await?;
            Ok(format!(
                "Pull-request diff for {reference} (git diff {range}):\n\n{diff}"
            ))
        }
        AuditScope::Path { root } => build_path_seed(root, workspace, ingot, vault, embedder).await,
    }
}

/// Seeds a path/whole-repo audit from a graph symbol listing plus a file tree.
async fn build_path_seed(
    root: &str,
    workspace: &Path,
    ingot: &IngotHandle,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn crate::embedder_port::Embedder>,
) -> Result<String, RpcError> {
    // A broad symbol query surfaces the repository's public surface. The graph
    // tool is read-only and returns an empty set when no graph is indexed.
    let graph_input = json!({ "query": root, "depth": 1 }).to_string();
    let symbols = execute_tool(
        "graph_query",
        &graph_input,
        workspace,
        None,
        ingot,
        vault,
        embedder,
    )
    .await;

    let list_input = json!({ "path": root }).to_string();
    let tree = execute_tool(
        "list_files",
        &list_input,
        workspace,
        None,
        ingot,
        vault,
        embedder,
    )
    .await;

    Ok(format!(
        "Path/repository audit scope: {root}\n\n\
         Symbol listing (graph_query):\n{symbols}\n\n\
         File tree (list_files):\n{tree}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ingot() -> IngotHandle {
        IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap())
    }

    fn vault() -> Arc<Mutex<Vault>> {
        Arc::new(Mutex::new(Vault::open_in_memory().unwrap()))
    }

    fn embedder() -> Arc<dyn crate::embedder_port::Embedder> {
        Arc::new(crate::embedder_port::FnvEmbedder::new())
    }

    // ── scope resolution ──────────────────────────────────────────────────────

    #[test]
    fn resolve_scope_defaults_to_diff() {
        assert_eq!(resolve_scope(&json!({})), AuditScope::Diff);
        assert_eq!(resolve_scope(&json!({ "diff": true })), AuditScope::Diff);
    }

    #[test]
    fn resolve_scope_path_arg_yields_path() {
        assert_eq!(
            resolve_scope(&json!({ "path": "src/lib.rs" })),
            AuditScope::Path {
                root: "src/lib.rs".to_owned()
            }
        );
    }

    #[test]
    fn resolve_scope_branch_yields_branch_with_head_default() {
        assert_eq!(
            resolve_scope(&json!({ "branch": "main" })),
            AuditScope::Branch {
                base: "main".to_owned(),
                head: "HEAD".to_owned()
            }
        );
    }

    #[test]
    fn resolve_scope_pr_yields_pr() {
        assert_eq!(
            resolve_scope(&json!({ "pr": "feature...main" })),
            AuditScope::Pr {
                reference: "feature...main".to_owned()
            }
        );
    }

    #[test]
    fn resolve_scope_pr_takes_precedence_over_branch_and_path() {
        let params = json!({ "pr": "x", "branch": "main", "path": "src" });
        assert_eq!(
            resolve_scope(&params),
            AuditScope::Pr {
                reference: "x".to_owned()
            }
        );
    }

    // ── seed building ─────────────────────────────────────────────────────────

    fn git_repo_with_change() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .output()
                .unwrap();
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(path.join("a.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        // An uncommitted change so `git diff HEAD` is non-empty.
        std::fs::write(path.join("a.txt"), "one\ntwo\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn diff_scope_seeds_from_unified_diff() {
        let repo = git_repo_with_change();
        let seed = build_seed(
            &AuditScope::Diff,
            repo.path(),
            &ingot(),
            &vault(),
            &embedder(),
        )
        .await
        .unwrap();
        assert!(!seed.trim().is_empty(), "seed must be non-empty");
        assert!(seed.contains("two"), "seed must contain the diff body");
    }

    #[tokio::test]
    async fn path_scope_seeds_from_graph_and_listing() {
        let repo = git_repo_with_change();
        let seed = build_seed(
            &AuditScope::Path {
                root: ".".to_owned(),
            },
            repo.path(),
            &ingot(),
            &vault(),
            &embedder(),
        )
        .await
        .unwrap();
        assert!(!seed.trim().is_empty(), "path seed must be non-empty");
        assert!(seed.contains("File tree"), "path seed must list files");
        assert!(seed.contains("a.txt"), "path seed must include the file");
    }

    #[tokio::test]
    async fn unresolvable_pr_ref_errors() {
        let repo = git_repo_with_change();
        let result = build_seed(
            &AuditScope::Pr {
                reference: "   ".to_owned(),
            },
            repo.path(),
            &ingot(),
            &vault(),
            &embedder(),
        )
        .await;
        assert!(result.is_err(), "empty PR ref must error");
    }
}
