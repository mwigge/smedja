//! Repo/PR/branch auditor: the `audit.run` RPC handler and its supporting
//! read-only exploration loop.
//!
//! The auditor runs the read-only Review role over a selected scope, exploring
//! the workspace with only `graph_query`, `read_file`, and `list_files`,
//! aggregating the model's output into structured [`findings::AuditFinding`]s.
//! Findings are de-duplicated, persisted as `smedja-ingot` `AuditEvent`s, and
//! rendered to a deterministic markdown report.
//!
//! The loop is genuinely read-only by two independent guarantees: it only ever
//! offers the read-only tool allowlist (any other tool call is rejected and fed
//! back as an error observation), and the session runs in `"review"` mode so the
//! existing `role_allows_write_bash` gate denies write-arity bash. The auditor
//! never constructs a `write_file`/`edit_file` dispatch.
//!
//! The implementation is split across adjacent submodules:
//! - [`scope`] — scope selection and seed-context building.
//! - [`findings`] — structured findings, parsing, dedup, and markdown rendering.
//! - [`persist`] — `AuditEvent` persistence.
//! - [`review_loop`] — the bounded read-only exploration loop and its `ReviewTurn`.
//! - [`provider`] — the provider-backed `ReviewTurn` implementation.
//! - [`run`] — the `audit.run` RPC entry point.

mod findings;
mod persist;
mod provider;
mod review_loop;
mod run;
mod scope;

pub(crate) use run::run;

/// Default upper bound on exploration iterations for one audit run.
pub(crate) const DEFAULT_MAX_ITERATIONS: u32 = 12;

/// Default token budget for one audit run (input + output, summed across turns).
pub(crate) const DEFAULT_TOKEN_BUDGET: u64 = 200_000;

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
