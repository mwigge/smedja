//! Repo/PR/branch auditor: the `audit.run` RPC handler and its supporting
//! read-only exploration loop.
//!
//! The auditor runs the read-only Review role over a selected scope, exploring
//! the workspace with only `graph_query`, `read_file`, and `list_files`,
//! aggregating the model's output into structured [`AuditFinding`]s. Findings
//! are de-duplicated, persisted as `smedja-ingot` `AuditEvent`s, and rendered to
//! a deterministic markdown report.
//!
//! The loop is genuinely read-only by two independent guarantees: it only ever
//! offers the read-only tool allowlist (any other tool call is rejected and fed
//! back as an error observation), and the session runs in `"review"` mode so the
//! existing `role_allows_write_bash` gate denies write-arity bash. The auditor
//! never constructs a `write_file`/`edit_file` dispatch.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smedja_adapter::types::Message as AdapterMessage;
use smedja_adapter::CallOptions;
use smedja_assayer::{Runner, Tier};
use smedja_bellows::event::CorrelationCtx;
use smedja_bellows::Dispatcher;
use smedja_ingot::{AuditEvent, IngotHandle, Session};
use smedja_rpc::{codes, RpcError};
use smedja_types::Timestamp;
use smedja_vault::Vault;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::executor::execute_tool;
use crate::handlers::HandlerState;
use crate::provider_pool::ProviderPool;

/// Default upper bound on exploration iterations for one audit run.
const DEFAULT_MAX_ITERATIONS: u32 = 12;

/// Default token budget for one audit run (input + output, summed across turns).
const DEFAULT_TOKEN_BUDGET: u64 = 200_000;

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

// ── Structured findings ──────────────────────────────────────────────────────

/// Severity of an [`AuditFinding`], ordered most-to-least severe for rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Severity {
    /// A critical defect (security hole, data loss, crash).
    Critical,
    /// A high-impact defect.
    High,
    /// A medium-impact defect.
    Medium,
    /// A low-impact defect or smell.
    Low,
    /// Informational note.
    Info,
}

impl Severity {
    /// Returns the lowercase wire string for this severity.
    #[must_use]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Info => "info",
        }
    }

    /// Returns the title-case section heading for this severity.
    #[must_use]
    fn heading(self) -> &'static str {
        match self {
            Self::Critical => "Critical",
            Self::High => "High",
            Self::Medium => "Medium",
            Self::Low => "Low",
            Self::Info => "Info",
        }
    }

    /// Parses a severity from a case-insensitive string.
    #[must_use]
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "critical" => Some(Self::Critical),
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            "info" | "informational" => Some(Self::Info),
            _ => None,
        }
    }

    /// The fixed rendering order, most severe first.
    const ORDER: [Self; 5] = [
        Self::Critical,
        Self::High,
        Self::Medium,
        Self::Low,
        Self::Info,
    ];
}

/// A single structured review finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AuditFinding {
    /// How severe the finding is.
    pub(crate) severity: Severity,
    /// Workspace-relative file the finding concerns.
    pub(crate) file: String,
    /// Optional 1-based line number.
    pub(crate) line: Option<u32>,
    /// Short rule slug (e.g. `error-handling`, `unwrap-in-lib`).
    pub(crate) rule: String,
    /// One-sentence rationale.
    pub(crate) rationale: String,
}

/// Parses a single finding from a JSON object, returning `None` when any
/// required field is missing or malformed (tolerant, non-fatal).
fn parse_finding_object(obj: &Value) -> Option<AuditFinding> {
    let severity = Severity::parse(obj.get("severity")?.as_str()?)?;
    let file = obj.get("file")?.as_str()?.trim();
    if file.is_empty() {
        return None;
    }
    let rule = obj.get("rule")?.as_str()?.trim();
    if rule.is_empty() {
        return None;
    }
    let rationale = obj.get("rationale")?.as_str()?.trim();
    if rationale.is_empty() {
        return None;
    }
    let line = obj
        .get("line")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    Some(AuditFinding {
        severity,
        file: file.to_owned(),
        line,
        rule: rule.to_owned(),
        rationale: rationale.to_owned(),
    })
}

/// Parses findings from model output, tolerantly skipping malformed objects.
///
/// Scans `text` for the first JSON array (optionally inside a ```` ```json ````
/// fence) and parses each element as an [`AuditFinding`]; elements that fail to
/// parse are skipped without failing the parse.
#[must_use]
pub(crate) fn parse_findings(text: &str) -> Vec<AuditFinding> {
    let Some(array) = first_json_array(text) else {
        return Vec::new();
    };
    array.iter().filter_map(parse_finding_object).collect()
}

/// Finds the first JSON array value embedded anywhere in `text`.
///
/// For each `[` byte a streaming deserializer attempts to read a JSON array,
/// ignoring trailing text. This tolerates fenced code blocks and surrounding
/// prose without a brace-counting scanner.
fn first_json_array(text: &str) -> Option<Vec<Value>> {
    use serde::de::Deserialize as _;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'[' {
            let mut de = serde_json::Deserializer::from_str(&text[i..]);
            if let Ok(Value::Array(arr)) = Value::deserialize(&mut de) {
                return Some(arr);
            }
        }
    }
    None
}

/// De-duplicates findings on `(file, line, rule)`, or `(file, rule)` when line
/// is absent. The first occurrence wins; its rationale is retained.
#[must_use]
pub(crate) fn dedup_findings(findings: Vec<AuditFinding>) -> Vec<AuditFinding> {
    let mut seen: HashSet<(String, Option<u32>, String)> = HashSet::new();
    let mut out = Vec::with_capacity(findings.len());
    for finding in findings {
        let key = (finding.file.clone(), finding.line, finding.rule.clone());
        if seen.insert(key) {
            out.push(finding);
        }
    }
    out
}

// ── Markdown report ──────────────────────────────────────────────────────────

/// Renders findings into a deterministic markdown report.
///
/// The report leads with a per-severity count header, then sections ordered
/// Critical → High → Medium → Low → Info; each finding renders as a
/// `` `file:line` — **rule** — rationale `` line. Findings are not re-ordered
/// within a severity, so identical input renders byte-identically.
#[must_use]
pub(crate) fn render_report(findings: &[AuditFinding]) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    out.push_str("# Audit Report\n\n");
    out.push_str("## Summary\n\n");
    for severity in Severity::ORDER {
        let count = findings.iter().filter(|f| f.severity == severity).count();
        let _ = writeln!(out, "- {}: {count}", severity.heading());
    }
    out.push('\n');

    for severity in Severity::ORDER {
        let matching: Vec<&AuditFinding> =
            findings.iter().filter(|f| f.severity == severity).collect();
        if matching.is_empty() {
            continue;
        }
        let _ = writeln!(out, "## {}\n", severity.heading());
        for finding in matching {
            let location = match finding.line {
                Some(line) => format!("{}:{line}", finding.file),
                None => finding.file.clone(),
            };
            let _ = writeln!(
                out,
                "- `{location}` — **{}** — {}",
                finding.rule, finding.rationale
            );
        }
        out.push('\n');
    }
    out
}

/// Returns the per-severity counts as a JSON object keyed by severity slug.
#[must_use]
pub(crate) fn severity_counts(findings: &[AuditFinding]) -> Value {
    let mut counts = serde_json::Map::new();
    for severity in Severity::ORDER {
        let count = findings.iter().filter(|f| f.severity == severity).count();
        counts.insert(severity.as_str().to_owned(), json!(count));
    }
    Value::Object(counts)
}

// ── Persistence ──────────────────────────────────────────────────────────────

/// Builds the `turn_start`/`turn_end` marker event for an audit run.
fn marker_event(session_id: &str, action_type: &str) -> AuditEvent {
    AuditEvent {
        id: Uuid::new_v4(),
        ts: Timestamp::now(),
        session_id: session_id.to_owned(),
        action_type: action_type.to_owned(),
        actor: "review".to_owned(),
        tier: Some("deep".to_owned()),
        ..AuditEvent::default()
    }
}

/// Builds the `audit_finding` event for a single finding.
///
/// Column mapping: `tool_name = rule`, `operation_name = severity`,
/// `error_kind = file`, with the rationale carried in the turn record.
fn finding_event(session_id: &str, finding: &AuditFinding) -> AuditEvent {
    AuditEvent {
        id: Uuid::new_v4(),
        ts: Timestamp::now(),
        session_id: session_id.to_owned(),
        action_type: "audit_finding".to_owned(),
        actor: "review".to_owned(),
        tool_name: Some(finding.rule.clone()),
        tier: Some("deep".to_owned()),
        operation_name: Some(finding.severity.as_str().to_owned()),
        error_kind: Some(finding.file.clone()),
        status: Some("ok".to_owned()),
        ..AuditEvent::default()
    }
}

/// Persists `turn_start`/`turn_end` markers around the findings.
///
/// Each finding is written as an `audit_finding` `AuditEvent`. Markers are
/// written even when the finding set is empty so the timeline view sees the run.
///
/// # Errors
///
/// Returns an [`RpcError`] when an ingot write fails.
pub(crate) async fn persist_findings(
    ingot: &IngotHandle,
    session_id: &str,
    findings: &[AuditFinding],
) -> Result<(), RpcError> {
    ingot
        .insert_audit_event(marker_event(session_id, "turn_start"))
        .await
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;
    for finding in findings {
        ingot
            .insert_audit_event(finding_event(session_id, finding))
            .await
            .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;
    }
    ingot
        .insert_audit_event(marker_event(session_id, "turn_end"))
        .await
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;
    Ok(())
}

// ── Read-only exploration loop ───────────────────────────────────────────────

/// Drives one review-role turn, returning the model's text output.
///
/// Abstracted as a trait so the loop can be exercised with a deterministic mock
/// in tests without a live provider.
pub(crate) trait ReviewTurn: Send + Sync {
    /// Runs one turn given the running transcript, returning the model text.
    fn run_turn(
        &self,
        transcript: &[AdapterMessage],
    ) -> impl std::future::Future<Output = Result<TurnOutput, RpcError>> + Send;
}

/// The result of one review turn: the model text and its token usage.
pub(crate) struct TurnOutput {
    /// The model's full text response for the turn.
    pub(crate) text: String,
    /// Input tokens consumed by the turn.
    pub(crate) input_tokens: u64,
    /// Output tokens produced by the turn.
    pub(crate) output_tokens: u64,
}

/// Bounds for one audit loop.
pub(crate) struct LoopBudget {
    /// Hard cap on exploration iterations.
    pub(crate) max_iterations: u32,
    /// Cap on summed input+output tokens.
    pub(crate) token_budget: u64,
}

impl Default for LoopBudget {
    fn default() -> Self {
        Self {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            token_budget: DEFAULT_TOKEN_BUDGET,
        }
    }
}

/// The system prompt steering the read-only auditor.
fn audit_system_prompt() -> String {
    format!(
        "You are a meticulous, read-only code auditor. Explore the codebase using \
         ONLY these tools: {tools}. You MUST NOT attempt to modify any file or run \
         any write command. To call a tool, emit a single JSON object \
         {{\"tool\": <name>, \"input\": {{...}}}}. When you have gathered enough \
         context, emit your findings as a fenced JSON array of objects with the \
         fields severity (critical|high|medium|low|info), file, line (optional \
         integer), rule (short slug), and rationale (one sentence). Emit the \
         findings array and stop.",
        tools = AUDIT_TOOLS.join(", ")
    )
}

/// Runs the bounded, read-only exploration loop.
///
/// Seed → review turn → optional allowed tool call (rejected if outside the
/// allowlist) → append observation → repeat, bounded by `budget`. Returns the
/// final de-duplicated findings.
///
/// The loop only ever dispatches tools in [`AUDIT_TOOLS`]; any other tool call
/// is rejected and fed back as an error observation, so no write tool is ever
/// constructed.
///
/// # Errors
///
/// Returns an [`RpcError`] when a review turn fails.
#[allow(clippy::too_many_arguments)] // forwards the read-only audit tool-loop dependencies
pub(crate) async fn run_audit_loop<R: ReviewTurn>(
    runner: &R,
    seed: &str,
    workspace: &Path,
    session: &Session,
    ingot: &IngotHandle,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn crate::embedder_port::Embedder>,
    budget: &LoopBudget,
) -> Result<Vec<AuditFinding>, RpcError> {
    debug_assert_eq!(
        session.mode.as_deref(),
        Some("review"),
        "audit loop must run in review mode"
    );

    let mut transcript = vec![
        AdapterMessage::system(audit_system_prompt()),
        AdapterMessage::user(format!("Audit the following scope.\n\n{seed}")),
    ];
    let mut spent_tokens = 0u64;
    let mut findings = Vec::new();

    for _iteration in 0..budget.max_iterations {
        if spent_tokens >= budget.token_budget {
            break;
        }
        let output = runner.run_turn(&transcript).await?;
        spent_tokens = spent_tokens.saturating_add(output.input_tokens);
        spent_tokens = spent_tokens.saturating_add(output.output_tokens);
        let response = output.text;

        // Any parseable findings array terminates the loop.
        let parsed = parse_findings(&response);
        if !parsed.is_empty() {
            findings = parsed;
            break;
        }

        transcript.push(AdapterMessage::assistant(response.clone()));

        let Some((tool_name, tool_input)) = crate::executor::parse_tool_call(&response) else {
            // No tool call and no findings: nothing more to explore.
            break;
        };

        // Read-only allowlist: reject anything outside AUDIT_TOOLS without
        // executing it, and feed the rejection back as an observation. A write
        // tool dispatch is never constructed.
        let observation = if is_audit_tool(&tool_name) {
            execute_tool(
                &tool_name,
                &tool_input,
                workspace,
                Some(session),
                ingot,
                vault,
                embedder,
            )
            .await
        } else {
            format!(
                "error: tool '{tool_name}' is not permitted in a read-only audit; \
                 allowed tools are {}",
                AUDIT_TOOLS.join(", ")
            )
        };
        transcript.push(AdapterMessage::user(format!("Observation:\n{observation}")));
    }

    Ok(dedup_findings(findings))
}

/// Provider-backed [`ReviewTurn`] that drives the real Review-role provider.
struct ProviderReviewTurn {
    pool: Arc<ProviderPool>,
    dispatcher: Arc<Dispatcher>,
    model_override: Option<String>,
}

impl ReviewTurn for ProviderReviewTurn {
    async fn run_turn(&self, transcript: &[AdapterMessage]) -> Result<TurnOutput, RpcError> {
        // The Review role routes to a deep provider; rotate over the eligible
        // ring until one serves the turn.
        let ring = self.pool.eligible_ring(Runner::Claude, Tier::Deep);
        if ring.is_empty() {
            return Err(RpcError::new(
                codes::INTERNAL_ERROR,
                "no LLM provider available for the review role",
            ));
        }

        // Split the system prompt out of the transcript for CallOptions.
        let system = transcript
            .iter()
            .find(|m| matches!(m.role, smedja_adapter::types::Role::System))
            .map(|m| m.content.clone());
        let body: Vec<AdapterMessage> = transcript
            .iter()
            .filter(|m| !matches!(m.role, smedja_adapter::types::Role::System))
            .cloned()
            .collect();

        let mut last_err = String::from("no provider attempt");
        for entry in ring {
            let model = self
                .model_override
                .clone()
                .or_else(|| std::env::var("SMEDJA_MODEL").ok())
                .unwrap_or_else(|| entry.default_model.clone());
            let opts = CallOptions {
                model,
                max_tokens: Some(2048),
                temperature: Some(0.2),
                system: system.clone(),
                tools: None,
                provider_session_id: None,
                smedja_session_id: None,
                permission_mode: None,
                stable_prefix_len: None,
                cache_strategy: smedja_adapter::CacheStrategy::None,
                workspace: None,
            };
            let stream = entry.provider.stream_chat(&body, &opts);
            let drained = tokio::time::timeout(
                std::time::Duration::from_mins(5),
                crate::common::drain_stream(
                    stream,
                    &self.dispatcher,
                    None,
                    &CorrelationCtx::default(),
                ),
            )
            .await;
            match drained {
                Ok(Ok((text, input_tokens, output_tokens, _cache_read, _session))) => {
                    return Ok(TurnOutput {
                        text,
                        input_tokens: u64::from(input_tokens),
                        output_tokens: u64::from(output_tokens),
                    });
                }
                Ok(Err(e)) => last_err = e.to_string(),
                Err(_) => "review turn timed out after 300s".clone_into(&mut last_err),
            }
        }
        Err(RpcError::new(
            codes::INTERNAL_ERROR,
            format!("review turn failed: {last_err}"),
        ))
    }
}

// ── audit.run handler ────────────────────────────────────────────────────────

/// Handles `audit.run`: resolve scope → seed → loop → dedup → persist → render.
///
/// Params: `{ workspace?, path?, branch?, head?, pr?, diff?, report?, format?,
/// max_iterations? }`.
/// Response: `{ findings, counts, report | report_path }`.
///
/// # Errors
///
/// Returns an [`RpcError`] when scope seeding fails (e.g. a `git` error or an
/// unresolvable pull-request reference) or persistence fails.
pub(crate) async fn run(state: HandlerState, params: Value) -> Result<Value, RpcError> {
    let workspace = resolve_workspace(&params);
    let scope = resolve_scope(&params);
    let seed = build_seed(
        &scope,
        &workspace,
        &state.ingot,
        &state.vault,
        &state.embedder,
    )
    .await?;

    // A read-only review-mode session is the second read-only guarantee.
    let session_id = Uuid::new_v4();
    let now = Timestamp::now();
    let session = Session {
        id: session_id,
        created_at: now,
        updated_at: now,
        status: "active".to_owned(),
        task_id: None,
        mode: Some("review".to_owned()),
        title: "audit".to_owned(),
        cowork_mode: false,
        workspace_root: Some(workspace.display().to_string()),
        model_override: None,
        runner_override: None,
    };
    state
        .ingot
        .create_session(session.clone())
        .await
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;

    let budget = LoopBudget {
        max_iterations: params
            .get("max_iterations")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(DEFAULT_MAX_ITERATIONS),
        token_budget: params
            .get("token_budget")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TOKEN_BUDGET),
    };

    let runner = ProviderReviewTurn {
        pool: Arc::clone(&state.provider_pool),
        dispatcher: Arc::clone(&state.dispatcher),
        model_override: None,
    };

    let findings = run_audit_loop(
        &runner,
        &seed,
        &workspace,
        &session,
        &state.ingot,
        &state.vault,
        &state.embedder,
        &budget,
    )
    .await?;

    persist_findings(&state.ingot, &session_id.to_string(), &findings).await?;

    respond(&params, &findings, &workspace).await
}

/// Builds the `audit.run` response, writing the report to `--report` when given.
async fn respond(
    params: &Value,
    findings: &[AuditFinding],
    workspace: &Path,
) -> Result<Value, RpcError> {
    let counts = severity_counts(findings);
    let format = params.get("format").and_then(Value::as_str).unwrap_or("md");

    if format == "json" {
        let typed = serde_json::to_value(findings)
            .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;
        return Ok(json!({ "findings": typed, "counts": counts }));
    }

    let report = render_report(findings);
    let typed = serde_json::to_value(findings)
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;

    if let Some(report_path) = params.get("report").and_then(Value::as_str) {
        if !report_path.is_empty() {
            let full = crate::executor::audit_report_path(workspace, report_path)
                .map_err(|e| RpcError::new(codes::INVALID_PARAMS, e))?;
            tokio::fs::write(&full, &report)
                .await
                .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, e.to_string()))?;
            return Ok(json!({
                "findings": typed,
                "counts": counts,
                "report_path": full.display().to_string(),
            }));
        }
    }

    Ok(json!({ "findings": typed, "counts": counts, "report": report }))
}

/// Resolves the workspace root from the `workspace` param, falling back to
/// `SMEDJA_WORKSPACE` and then the current directory.
fn resolve_workspace(params: &Value) -> std::path::PathBuf {
    if let Some(ws) = params.get("workspace").and_then(Value::as_str) {
        if !ws.is_empty() {
            return std::path::PathBuf::from(ws);
        }
    }
    std::env::var("SMEDJA_WORKSPACE")
        .ok()
        .filter(|p| !p.is_empty())
        .map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            std::path::PathBuf::from,
        )
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

    fn review_session() -> Session {
        Session {
            id: Uuid::new_v4(),
            created_at: Timestamp::from_micros(0),
            updated_at: Timestamp::from_micros(0),
            status: "active".to_owned(),
            task_id: None,
            mode: Some("review".to_owned()),
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        }
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

    // ── finding parsing ───────────────────────────────────────────────────────

    #[test]
    fn parses_fenced_json_array_of_findings() {
        let text = "Here are my findings:\n```json\n[\
            {\"severity\":\"high\",\"file\":\"src/a.rs\",\"line\":12,\"rule\":\"unwrap-in-lib\",\"rationale\":\"uses unwrap\"}\
            ]\n```\nDone.";
        let findings = parse_findings(text);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::High);
        assert_eq!(findings[0].file, "src/a.rs");
        assert_eq!(findings[0].line, Some(12));
        assert_eq!(findings[0].rule, "unwrap-in-lib");
    }

    #[test]
    fn skips_malformed_finding_keeps_valid_siblings() {
        let text = "[\
            {\"severity\":\"bogus\",\"file\":\"x\",\"rule\":\"r\",\"rationale\":\"y\"},\
            {\"file\":\"only-file\"},\
            {\"severity\":\"low\",\"file\":\"src/b.rs\",\"rule\":\"naming\",\"rationale\":\"unclear name\"}\
            ]";
        let findings = parse_findings(text);
        assert_eq!(findings.len(), 1, "only the valid finding survives");
        assert_eq!(findings[0].file, "src/b.rs");
    }

    #[test]
    fn dedup_on_file_line_rule_first_wins() {
        let findings = vec![
            AuditFinding {
                severity: Severity::High,
                file: "a.rs".to_owned(),
                line: Some(3),
                rule: "r".to_owned(),
                rationale: "first".to_owned(),
            },
            AuditFinding {
                severity: Severity::Low,
                file: "a.rs".to_owned(),
                line: Some(3),
                rule: "r".to_owned(),
                rationale: "second".to_owned(),
            },
        ];
        let deduped = dedup_findings(findings);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].rationale, "first", "first occurrence wins");
    }

    #[test]
    fn dedup_on_file_rule_when_line_absent() {
        let findings = vec![
            AuditFinding {
                severity: Severity::High,
                file: "a.rs".to_owned(),
                line: None,
                rule: "r".to_owned(),
                rationale: "first".to_owned(),
            },
            AuditFinding {
                severity: Severity::High,
                file: "a.rs".to_owned(),
                line: None,
                rule: "r".to_owned(),
                rationale: "second".to_owned(),
            },
        ];
        assert_eq!(dedup_findings(findings).len(), 1);
    }

    // ── report rendering ──────────────────────────────────────────────────────

    fn sample_findings() -> Vec<AuditFinding> {
        vec![
            AuditFinding {
                severity: Severity::Critical,
                file: "src/a.rs".to_owned(),
                line: Some(10),
                rule: "sql-injection".to_owned(),
                rationale: "interpolated SQL".to_owned(),
            },
            AuditFinding {
                severity: Severity::Low,
                file: "src/b.rs".to_owned(),
                line: None,
                rule: "naming".to_owned(),
                rationale: "abbreviated name".to_owned(),
            },
        ]
    }

    #[test]
    fn report_has_count_header_and_severity_sections() {
        let report = render_report(&sample_findings());
        assert!(report.contains("## Summary"), "must have a summary header");
        assert!(report.contains("Critical: 1"), "must count critical");
        assert!(report.contains("Low: 1"), "must count low");
        assert!(
            report.contains("## Critical"),
            "must have a Critical section"
        );
        assert!(
            report.contains("`src/a.rs:10` — **sql-injection** — interpolated SQL"),
            "must render the finding line; got:\n{report}"
        );
        assert!(
            report.contains("`src/b.rs` — **naming** — abbreviated name"),
            "lineless finding must render without a colon; got:\n{report}"
        );
        // Critical section must precede Low.
        let crit = report.find("## Critical").unwrap();
        let low = report.find("## Low").unwrap();
        assert!(crit < low, "Critical must precede Low");
    }

    #[test]
    fn report_is_deterministic() {
        let findings = sample_findings();
        assert_eq!(render_report(&findings), render_report(&findings));
    }

    #[test]
    fn severity_counts_covers_all_levels() {
        let counts = severity_counts(&sample_findings());
        assert_eq!(counts["critical"], 1);
        assert_eq!(counts["high"], 0);
        assert_eq!(counts["low"], 1);
    }

    // ── persistence ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn persists_findings_as_audit_events_with_markers() {
        let ig = ingot();
        let sid = Uuid::new_v4().to_string();
        let findings = sample_findings();
        persist_findings(&ig, &sid, &findings).await.unwrap();

        let events = ig.list_audit_events(&sid).await.unwrap();
        let starts = events
            .iter()
            .filter(|e| e.action_type == "turn_start")
            .count();
        let ends = events
            .iter()
            .filter(|e| e.action_type == "turn_end")
            .count();
        assert_eq!(starts, 1, "exactly one turn_start marker");
        assert_eq!(ends, 1, "exactly one turn_end marker");

        let finding_events: Vec<&AuditEvent> = events
            .iter()
            .filter(|e| e.action_type == "audit_finding")
            .collect();
        assert_eq!(finding_events.len(), 2);
        let crit = finding_events
            .iter()
            .find(|e| e.error_kind.as_deref() == Some("src/a.rs"))
            .unwrap();
        assert_eq!(crit.actor, "review");
        assert_eq!(crit.tier.as_deref(), Some("deep"));
        assert_eq!(crit.tool_name.as_deref(), Some("sql-injection"));
        assert_eq!(crit.operation_name.as_deref(), Some("critical"));
    }

    #[tokio::test]
    async fn zero_findings_still_persists_markers() {
        let ig = ingot();
        let sid = Uuid::new_v4().to_string();
        persist_findings(&ig, &sid, &[]).await.unwrap();
        let events = ig.list_audit_events(&sid).await.unwrap();
        assert_eq!(events.len(), 2, "two markers, no findings");
        assert!(events.iter().any(|e| e.action_type == "turn_start"));
        assert!(events.iter().any(|e| e.action_type == "turn_end"));
    }

    // ── read-only loop ────────────────────────────────────────────────────────

    /// A scripted runner that returns each response in order, recording the
    /// observations it was fed back so a test can assert on rejection text.
    struct ScriptedRunner {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedRunner {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: std::sync::Mutex::new(
                    responses.into_iter().map(ToOwned::to_owned).collect(),
                ),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl ReviewTurn for ScriptedRunner {
        async fn run_turn(&self, transcript: &[AdapterMessage]) -> Result<TurnOutput, RpcError> {
            if let Some(last) = transcript.last() {
                self.seen.lock().unwrap().push(last.content.clone());
            }
            let text = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default();
            Ok(TurnOutput {
                text,
                input_tokens: 10,
                output_tokens: 10,
            })
        }
    }

    #[tokio::test]
    async fn loop_rejects_non_allowlisted_tool_as_error_observation() {
        // First turn asks for a forbidden write_file tool; second turn emits
        // findings. The loop must feed back a rejection and never execute it.
        let runner = ScriptedRunner::new(vec![
            r#"{"tool":"write_file","input":{"path":"x","content":"y"}}"#,
            r#"[{"severity":"info","file":"a.rs","rule":"note","rationale":"ok"}]"#,
        ]);
        let session = review_session();
        let ws = tempfile::tempdir().unwrap();
        // Pre-create a file the forbidden write would have targeted.
        std::fs::write(ws.path().join("x"), "original").unwrap();

        let findings = run_audit_loop(
            &runner,
            "seed",
            ws.path(),
            &session,
            &ingot(),
            &vault(),
            &embedder(),
            &LoopBudget::default(),
        )
        .await
        .unwrap();

        assert_eq!(findings.len(), 1, "loop completes with the findings turn");
        // The file must be untouched: the write was never dispatched.
        let content = std::fs::read_to_string(ws.path().join("x")).unwrap();
        assert_eq!(content, "original", "forbidden write must not execute");
        // The rejection must have been fed back as an observation.
        let seen = runner.seen.lock().unwrap();
        assert!(
            seen.iter().any(|s| s.contains("not permitted")),
            "a rejection observation must be fed back; saw: {seen:?}"
        );
    }

    #[tokio::test]
    async fn loop_dispatches_allowlisted_tool() {
        let runner = ScriptedRunner::new(vec![
            r#"{"tool":"list_files","input":{"path":"."}}"#,
            r#"[{"severity":"low","file":"f.rs","rule":"r","rationale":"x"}]"#,
        ]);
        let session = review_session();
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("f.rs"), "fn main() {}").unwrap();

        let findings = run_audit_loop(
            &runner,
            "seed",
            ws.path(),
            &session,
            &ingot(),
            &vault(),
            &embedder(),
            &LoopBudget::default(),
        )
        .await
        .unwrap();
        assert_eq!(findings.len(), 1);
        let seen = runner.seen.lock().unwrap();
        assert!(
            seen.iter().any(|s| s.contains("f.rs")),
            "list_files observation must be fed back; saw: {seen:?}"
        );
    }

    #[tokio::test]
    async fn loop_halts_at_max_iterations() {
        // The runner never emits findings: an endless tool-call loop. The
        // iteration bound must stop it.
        let responses: Vec<&str> =
            std::iter::repeat_n(r#"{"tool":"list_files","input":{"path":"."}}"#, 100).collect();
        let runner = ScriptedRunner::new(responses);
        let session = review_session();
        let ws = tempfile::tempdir().unwrap();

        let budget = LoopBudget {
            max_iterations: 3,
            token_budget: DEFAULT_TOKEN_BUDGET,
        };
        let findings = run_audit_loop(
            &runner,
            "seed",
            ws.path(),
            &session,
            &ingot(),
            &vault(),
            &embedder(),
            &budget,
        )
        .await
        .unwrap();
        assert!(findings.is_empty(), "no findings when none are emitted");
        // The runner was called at most max_iterations times.
        let calls = runner.seen.lock().unwrap().len();
        assert!(
            calls <= 3,
            "loop must halt at max_iterations; calls={calls}"
        );
    }

    // ── respond / report writing ──────────────────────────────────────────────

    #[tokio::test]
    async fn respond_inline_returns_report_and_counts() {
        let ws = tempfile::tempdir().unwrap();
        let resp = respond(&json!({}), &sample_findings(), ws.path())
            .await
            .unwrap();
        assert!(
            resp.get("report").is_some(),
            "inline report must be present"
        );
        assert!(resp["report"].as_str().unwrap().contains("## Summary"));
        assert_eq!(resp["counts"]["critical"], 1);
        assert!(resp["findings"].is_array());
    }

    #[tokio::test]
    async fn respond_format_json_emits_typed_findings_without_loss() {
        let ws = tempfile::tempdir().unwrap();
        let resp = respond(&json!({ "format": "json" }), &sample_findings(), ws.path())
            .await
            .unwrap();
        assert!(
            resp.get("report").is_none(),
            "json format has no markdown body"
        );
        let arr = resp["findings"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["severity"], "critical");
        assert_eq!(arr[0]["file"], "src/a.rs");
        assert_eq!(arr[0]["line"], 10);
        assert_eq!(arr[0]["rule"], "sql-injection");
        assert_eq!(arr[0]["rationale"], "interpolated SQL");
    }

    #[tokio::test]
    async fn respond_writes_report_to_path() {
        let ws = tempfile::tempdir().unwrap();
        // Canonicalise so the workspace-boundary check (which canonicalises the
        // root) accepts the not-yet-existing report path on platforms where the
        // temp dir is itself a symlink (e.g. /var → /private/var on macOS).
        let ws_root = ws.path().canonicalize().unwrap();
        let resp = respond(
            &json!({ "report": "report.md" }),
            &sample_findings(),
            &ws_root,
        )
        .await
        .unwrap();
        let path = resp["report_path"].as_str().unwrap();
        let written = std::fs::read_to_string(path).unwrap();
        assert!(
            written.contains("## Summary"),
            "report file must hold markdown"
        );
        assert!(
            resp.get("report").is_none(),
            "path mode does not inline the body"
        );
        assert_eq!(resp["counts"]["low"], 1);
    }

    #[tokio::test]
    async fn full_pipeline_loop_persist_render_produces_markdown_report() {
        // End-to-end over the daemon side without a live provider: a scripted
        // review turn surfaces findings, which are persisted and rendered to a
        // markdown report with the per-severity header and severity sections.
        let runner = ScriptedRunner::new(vec![
            r#"{"tool":"list_files","input":{"path":"."}}"#,
            r#"[
                {"severity":"critical","file":"src/db.rs","line":7,"rule":"sql-injection","rationale":"interpolated query"},
                {"severity":"low","file":"src/util.rs","rule":"naming","rationale":"unclear name"}
            ]"#,
        ]);
        let session = review_session();
        let ws = tempfile::tempdir().unwrap();
        let ws_root = ws.path().canonicalize().unwrap();
        std::fs::write(ws_root.join("src.rs"), "fn main() {}").unwrap();
        let ig = ingot();

        let findings = run_audit_loop(
            &runner,
            "audit src/",
            &ws_root,
            &session,
            &ig,
            &vault(),
            &embedder(),
            &LoopBudget::default(),
        )
        .await
        .unwrap();
        assert_eq!(findings.len(), 2);

        persist_findings(&ig, &session.id.to_string(), &findings)
            .await
            .unwrap();
        let events = ig.list_audit_events(&session.id.to_string()).await.unwrap();
        assert!(events.iter().any(|e| e.action_type == "audit_finding"));

        let report = render_report(&findings);
        assert!(report.contains("## Summary"), "header present");
        assert!(report.contains("Critical: 1"));
        assert!(report.contains("## Critical"));
        assert!(report.contains("`src/db.rs:7` — **sql-injection**"));
    }

    #[test]
    fn review_session_denies_write_bash() {
        // The session the loop runs under is in review mode, so the existing
        // gate denies write-arity bash.
        let session = review_session();
        assert!(
            !crate::executor::role_allows_write_bash_for_test(&session),
            "review-mode session must deny write-arity bash"
        );
    }
}
