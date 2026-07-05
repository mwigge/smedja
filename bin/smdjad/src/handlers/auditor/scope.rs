//! Audit scope resolution and seed building: the read-only tool allowlist,
//! scope parsing, git/PR reference resolution, and diff/path seed construction.
//! Moved verbatim from `auditor.rs`.

use super::*;

/// The exact set of tools the read-only audit loop may dispatch.
///
/// Any tool call outside this set is rejected without execution and fed back to
/// the model as an error observation. This is the structural read-only guarantee
/// on top of the `"review"`-mode `role_allows_write_bash` gate.
pub(crate) const AUDIT_TOOLS: &[&str] = &["graph_query", "read_file", "list_files"];

/// Returns `true` when `tool_name` is in the read-only audit allowlist.
#[must_use]
pub(crate) fn is_audit_tool(tool_name: &str) -> bool {
    AUDIT_TOOLS.contains(&tool_name)
}

// ── Scope selection ──────────────────────────────────────────────────────────

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
        None,
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
        None,
    )
    .await;

    Ok(format!(
        "Path/repository audit scope: {root}\n\n\
         Symbol listing (graph_query):\n{symbols}\n\n\
         File tree (list_files):\n{tree}"
    ))
}
