//! Human-in-the-loop gate for tool calls in cowork mode.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use smedja_bellows::{CorrelationCtx, Dispatcher, TurnEvent};
use tokio::sync::{oneshot, Mutex};

/// How long a gated tool call waits for a human decision before failing closed
/// (deny). Shared by every suspend path — the native gate, the ACP
/// `session/request_permission` bridge, and @shell fragments.
pub const APPROVAL_TIMEOUT_SECS: u64 = 30 * 60;

/// Describes a pending tool call awaiting human approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalPrompt {
    pub step_n: u32,
    pub tool: String,
    /// Args with secret values redacted by [`scrub_args`] — this is the copy
    /// shown in the popup, the NDJSON stream, and `cowork.pending` responses.
    /// The raw args never leave the caller (execution uses its own copy).
    pub args_scrubbed: serde_json::Value,
    pub reasoning: String,
    pub plan_summary: String,
    /// Name of the agent/runner that raised the call, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Workspace root the call runs in, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Whether the backend that raised this prompt can consume a *modify*
    /// decision (replacement args). `false` for one-shot backends (@shell) and
    /// the ACP tool-gate adapter, which has no channel to hand modified input
    /// back — a modify request there is rejected with an explicit error.
    #[serde(default = "default_supports_modify")]
    pub supports_modify: bool,
}

fn default_supports_modify() -> bool {
    true
}

impl ApprovalPrompt {
    /// Maximum serialised size of the scrubbed display args. A megabyte-scale
    /// `tool_input` would otherwise be cloned into the pending map AND
    /// published on the NDJSON stream; the display copy is capped instead
    /// (execution always uses the raw args upstream).
    const MAX_ARGS_DISPLAY: usize = 8 * 1024;

    /// Builds a prompt from RAW tool args: the display copy is scrubbed via
    /// [`scrub_args`], capped at [`Self::MAX_ARGS_DISPLAY`] serialised chars,
    /// and an empty `reasoning` is derived from the tool + args.
    #[must_use]
    pub fn new(step_n: u32, tool: &str, raw_args: &serde_json::Value, reasoning: &str) -> Self {
        let scrubbed = cap_args_display(scrub_args(raw_args));
        let reasoning = if reasoning.is_empty() {
            derive_reasoning(tool, &scrubbed)
        } else {
            reasoning.to_owned()
        };
        Self {
            step_n,
            tool: tool.to_owned(),
            args_scrubbed: scrubbed,
            reasoning,
            plan_summary: String::new(),
            agent: None,
            cwd: None,
            supports_modify: true,
        }
    }
}

/// Caps an already-scrubbed args value at [`ApprovalPrompt::MAX_ARGS_DISPLAY`]
/// serialised characters. Oversized payloads are replaced by a plain truncated
/// string with a `…[truncated]` marker (no longer valid JSON — display only).
fn cap_args_display(scrubbed: serde_json::Value) -> serde_json::Value {
    let s = scrubbed.to_string();
    if s.chars().count() <= ApprovalPrompt::MAX_ARGS_DISPLAY {
        return scrubbed;
    }
    let cut: String = s.chars().take(ApprovalPrompt::MAX_ARGS_DISPLAY).collect();
    serde_json::Value::String(format!("{cut}…[truncated]"))
}

/// Per-call context threaded into the gate: who is asking and where the call
/// runs (the workspace drives `[[permission.rules]]` loading and allow-always
/// persistence).
#[derive(Debug, Clone)]
pub struct GateContext {
    /// Name of the agent/runner that raised the call (surfaced in the popup).
    pub agent: Option<String>,
    /// Workspace root for rule loading/persistence and the popup's cwd field.
    pub workspace: Option<std::path::PathBuf>,
    /// `false` for backends with no modify channel (@shell fragments, the ACP
    /// tool-gate adapter): a modify request is then rejected with an explicit
    /// error instead of silently degrading to a deny.
    pub supports_modify: bool,
}

impl Default for GateContext {
    fn default() -> Self {
        Self {
            agent: None,
            workspace: None,
            supports_modify: true,
        }
    }
}

/// Why a `cowork.modify` request was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModifyRejection {
    /// No pending approval with this id.
    UnknownId,
    /// The backend that raised the prompt cannot consume replacement args.
    Unsupported { tool: String },
    /// The instruction is not a JSON object of replacement arguments.
    InvalidInstruction(String),
}

impl std::fmt::Display for ModifyRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownId => write!(f, "no pending approval with that id"),
            Self::Unsupported { tool } => write!(
                f,
                "modify is not supported for this backend (tool `{tool}`); approve or deny instead"
            ),
            Self::InvalidInstruction(detail) => write!(
                f,
                "modify requires replacement arguments as a JSON object: {detail}"
            ),
        }
    }
}

/// Redacts secrets from a tool-call args value for display: values of keys
/// whose name contains a secret-ish word (token, key, secret, password,
/// authorization, credential — case-insensitive) and string values that look
/// like high-entropy bearer tokens are replaced with `"[redacted]"`. Recursive
/// over objects and arrays.
#[must_use]
pub fn scrub_args(args: &serde_json::Value) -> serde_json::Value {
    match args {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| {
                    if is_secret_key(k) {
                        (
                            k.clone(),
                            serde_json::Value::String("[redacted]".to_owned()),
                        )
                    } else {
                        (k.clone(), scrub_args(v))
                    }
                })
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(scrub_args).collect())
        }
        serde_json::Value::String(s) if looks_like_bearer(s) => {
            serde_json::Value::String("[redacted]".to_owned())
        }
        other => other.clone(),
    }
}

/// True when a key name suggests a secret value.
fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [
        "token",
        "key",
        "secret",
        "password",
        "authorization",
        "credential",
    ]
    .iter()
    .any(|pat| k.contains(pat))
}

/// True when a string looks like a bearer token. Catches, in order:
///
/// * known credential prefixes (`sk-`, `ghp_`, `gho_`, `xoxb-`, `xoxp-`,
///   `AKIA`, `eyJ` JWTs) regardless of length or entropy;
/// * pure-hex strings of 32+ chars (case-insensitive) — hex entropy is ~4.0
///   bits/byte, below the entropy threshold, so they need their own rule;
/// * otherwise: long, whitespace-free, path-free, and high-entropy (so
///   ordinary paths, commands, and UUIDs pass).
fn looks_like_bearer(s: &str) -> bool {
    const MIN_LEN: usize = 24;
    const MIN_ENTROPY: f64 = 4.2;
    const KNOWN_PREFIXES: &[&str] = &["sk-", "ghp_", "gho_", "xoxb-", "xoxp-", "AKIA", "eyJ"];
    if KNOWN_PREFIXES.iter().any(|p| s.starts_with(p)) {
        return true;
    }
    if s.len() >= 32 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return true;
    }
    if s.len() < MIN_LEN || s.contains(char::is_whitespace) || s.contains('/') {
        return false;
    }
    shannon_entropy(s.as_bytes()) >= MIN_ENTROPY
}

/// True when a command/path payload carries a secret-looking value: either the
/// payload as a whole is a bare token, or any shell word inside it is (so
/// `curl -H "Authorization: Bearer sk-…"` is caught). Used to refuse writing an
/// allow-always rule that would persist the secret to `.smedja/workspace.toml`.
fn payload_contains_secret(payload: &str) -> bool {
    if looks_like_bearer(payload) {
        return true;
    }
    payload.split_whitespace().any(|word| {
        let word = word.trim_matches(|c| c == '"' || c == '\'');
        looks_like_bearer(word)
    })
}

/// Whether an allow-always resolution may persist a `[[permission.rules]]`
/// Allow entry for a call with these (RAW) args. Returns `Some(note)` — a
/// human-readable reason for the gate response — when the rule must NOT be
/// written:
///
/// * the args carry no `command`/`cmd`/`path` to scope the rule to, so
///   persisting would blanket-allow every future call of the tool;
/// * the command contains a `*`, which would silently turn into a prefix
///   pattern on evaluation (over-broad allow); or
/// * the scoped payload contains a secret-looking value, which must never be
///   written to `.smedja/workspace.toml`.
///
/// In both cases the single approval still proceeds — only persistence is
/// skipped.
#[must_use]
pub fn allow_always_skip_reason(args: &serde_json::Value) -> Option<&'static str> {
    let command = args
        .get("command")
        .or_else(|| args.get("cmd"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    if command.is_none() && path.is_none() {
        return Some("always not available for this tool (no command/path to scope the rule)");
    }
    if command.is_some_and(|c| c.contains('*')) {
        return Some("rule not persisted: command contains a glob character");
    }
    if command.is_some_and(payload_contains_secret) || path.is_some_and(payload_contains_secret) {
        return Some("rule not persisted: command contains a secret-looking value");
    }
    None
}

/// Shannon entropy in bits per byte.
fn shannon_entropy(bytes: &[u8]) -> f64 {
    let mut counts = [0usize; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let n = bytes.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

/// Derives a one-line rationale for the approval popup when the caller
/// supplied none: the command for shell tools, the path for file tools, else a
/// truncated compact rendering of the args. Expects already-scrubbed args.
fn derive_reasoning(tool: &str, args: &serde_json::Value) -> String {
    const MAX: usize = 160;
    let raw = if let Some(cmd) = args
        .get("command")
        .or_else(|| args.get("cmd"))
        .and_then(|v| v.as_str())
    {
        format!("run: {cmd}")
    } else if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        format!("{tool}: {path}")
    } else if args.is_null() {
        return String::new();
    } else {
        serde_json::to_string(args).unwrap_or_default()
    };
    if raw.chars().count() > MAX {
        let truncated: String = raw.chars().take(MAX).collect();
        format!("{truncated}…")
    } else {
        raw
    }
}

/// The human's decision on a pending tool call.
#[derive(Debug, Clone)]
pub enum Decision {
    Approve,
    Deny(String),
    Modify(String),
}

/// Outcome of a gated tool call ([`CoworkGate::gate_tool`] /
/// [`CoworkGate::gate_tool_forced_ask`]).
#[derive(Debug, Clone)]
pub struct GateOutcome {
    /// The gate's decision.
    pub decision: Decision,
    /// Set when an *allow-always* resolution could not be persisted (see
    /// [`allow_always_skip_reason`]); surface it so the user knows the choice
    /// applies to this call only.
    pub note: Option<String>,
}

/// Unique ID for a pending approval request.
pub type ApprovalId = String;

/// A pending approval awaiting a human decision.
struct PendingApproval {
    prompt: ApprovalPrompt,
    /// Sender half of the oneshot; the receiver suspends in [`CoworkGate::intercept`].
    tx: oneshot::Sender<Decision>,
    /// The dispatcher the `CoworkRequest` was pushed on, cloned so
    /// [`CoworkGate::resolve`] (and the timeout path) can broadcast the matching
    /// [`TurnEvent::CoworkResolved`] — without it, resolutions never reach the
    /// stream and stale prompts replay on every reconnect.
    notify: Option<Dispatcher>,
}

/// RAII guard that removes a pending entry from the map on **every** exit path
/// of [`CoworkGate::intercept`] — normal resolution, timeout, sender-dropped, or
/// future cancellation (TUI disconnect / walk-away). Without this, the timeout
/// and sender-dropped branches returned `Deny` while leaving the map entry
/// behind, leaking one entry per unanswered prompt forever.
///
/// When the guard removes a STILL-pending entry (caller cancelled — the entry
/// was not consumed by [`CoworkGate::resolve`] or the timeout path), it
/// broadcasts [`TurnEvent::CoworkResolved`] with
/// [`smedja_bellows::CoworkOutcome::Cancelled`] on the request's dispatcher, so
/// stream clients dismiss the overlay instead of leaving a ghost prompt up
/// forever.
struct PendingGuard {
    pending: Arc<Mutex<HashMap<ApprovalId, PendingApproval>>>,
    id: ApprovalId,
}

impl PendingGuard {
    /// Removes `id` from the map; if the entry was still pending and carried a
    /// notify dispatcher, publishes the `Cancelled` resolution.
    fn remove_and_notify_cancelled(map: &mut HashMap<ApprovalId, PendingApproval>, id: &str) {
        if let Some(entry) = map.remove(id) {
            if let Some(dispatcher) = entry.notify {
                dispatcher.publish(TurnEvent::CoworkResolved {
                    approval_id: id.to_owned(),
                    outcome: smedja_bellows::CoworkOutcome::Cancelled,
                });
            }
        }
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        // Fast path: uncontended lock, remove synchronously. If the map is
        // momentarily locked (e.g. a concurrent resolve), offload the removal to
        // the runtime so Drop never blocks. `intercept` always runs on a Tokio
        // runtime, so `tokio::spawn` is available.
        if let Ok(mut map) = self.pending.try_lock() {
            Self::remove_and_notify_cancelled(&mut map, &self.id);
        } else {
            let pending = Arc::clone(&self.pending);
            let id = std::mem::take(&mut self.id);
            tokio::spawn(async move {
                let mut map = pending.lock().await;
                Self::remove_and_notify_cancelled(&mut map, &id);
            });
        }
    }
}

/// Intercepts tool calls when cowork mode is active.
///
/// One `CoworkGate` per session. External RPC calls (`cowork.approve`,
/// `cowork.deny`, `cowork.modify`) send decisions through the channel.
///
/// Codex-backed sessions that manage their own approval loop skip `intercept`
/// entirely at the call site rather than using a bypass flag on the gate.
#[derive(Default)]
pub struct CoworkGate {
    pending: Arc<Mutex<HashMap<ApprovalId, PendingApproval>>>,
    /// Per-session permission mode driving the gate policy (Shift+Tab cycles it
    /// from the TUI). Defaults to [`PermissionMode::Ask`].
    mode: Arc<Mutex<PermissionMode>>,
    /// Approval ids the user resolved with an *allow-always* scope, so the
    /// resolution site (e.g. the industry-ACP `session/request_permission`
    /// bridge in `acp.rs`) can persist a matching `[[permission.rules]]` Allow
    /// entry. Additive and out-of-band: the native tool loop never reads it, so
    /// the 3-way [`Decision`] enum keeps its exact shape. An id is inserted by
    /// [`Self::approve_always`] and consumed by [`Self::take_always`].
    always_ids: Arc<Mutex<HashSet<ApprovalId>>>,
    /// Persistence-veto notes captured by [`Self::approve_always`] (computed
    /// from the scrubbed prompt BEFORE resolution), consumed by the suspend
    /// path via [`Self::take_always_note`] so the gate response can explain
    /// why an allow-always was not persisted.
    always_notes: Arc<Mutex<HashMap<ApprovalId, String>>>,
}

/// Per-session permission mode controlling how mutating tool calls are gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Stop and ask before every mutation (edit/write/shell). The default.
    #[default]
    Ask,
    /// Auto-approve known file edits; still ask before shell/unknown tools.
    AcceptEdits,
    /// Read-only: deny all mutations (the agent may only read/analyse/plan).
    Plan,
    /// Auto-approve everything (no gate).
    Auto,
}

impl PermissionMode {
    /// Parses a mode name leniently; anything unrecognised falls back to `Ask`.
    #[must_use]
    pub fn parse_lenient(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "accept_edits" | "acceptedits" | "edits" => Self::AcceptEdits,
            "plan" => Self::Plan,
            "auto" => Self::Auto,
            _ => Self::Ask,
        }
    }

    /// Stable lowercase identifier (round-trips with [`Self::parse_lenient`]).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AcceptEdits => "accept_edits",
            Self::Plan => "plan",
            Self::Auto => "auto",
        }
    }

    /// Next mode in the `Shift+Tab` cycle: `Ask` → `AcceptEdits` → `Plan` → `Auto` → `Ask`.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Ask => Self::AcceptEdits,
            Self::AcceptEdits => Self::Plan,
            Self::Plan => Self::Auto,
            Self::Auto => Self::Ask,
        }
    }
}

/// The policy's verdict for a single tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Run the tool without asking.
    Allow,
    /// Block the tool outright (e.g. a mutation in `Plan` mode).
    Deny,
    /// Suspend on the cowork gate for a human decision.
    Ask,
}

/// Coarse risk class of a tool, by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    /// Read-only (never gated).
    ReadOnly,
    /// A known file mutation (auto-approved in `AcceptEdits`).
    Edit,
    /// Shell/command execution *or* an unknown tool — always needs explicit
    /// approval outside `Auto` (fail-safe: unknown tools are treated as exec).
    Exec,
}

fn tool_kind(tool: &str) -> ToolKind {
    let t = tool.to_ascii_lowercase();
    // Shell / arbitrary command execution — the most dangerous class.
    if t.contains("bash")
        || t.contains("shell")
        || t.contains("run_command")
        || t == "exec"
        || t.starts_with("exec_")
    {
        return ToolKind::Exec;
    }
    // Read-only tools (the daemon's read-safe set plus common read verbs).
    #[allow(clippy::items_after_statements)]
    const READ: &[&str] = &[
        "read_file",
        "list_files",
        "smedja_vault_search",
        "smedja_retrieve",
        "graph_query",
        "otel_query",
        "metric_query",
        "log_tail",
        "lsp_definition",
        "lsp_references",
        "lsp_hover",
        "lsp_document_symbols",
        "lsp_workspace_symbols",
    ];
    if READ.contains(&t.as_str())
        || t.starts_with("read")
        || t.starts_with("list")
        || t.starts_with("get")
        || t.starts_with("search")
        || t.starts_with("query")
        || t.starts_with("grep")
        || t.starts_with("glob")
        || t.starts_with("view")
    {
        return ToolKind::ReadOnly;
    }
    // Known mutating edit tools.
    #[allow(clippy::items_after_statements)]
    const EDIT: &[&str] = &[
        "write_file",
        "edit_file",
        "smedja_vault_store",
        "apply_patch",
        "str_replace",
        "create_file",
        "delete_file",
        "lsp_rename_symbol",
        "write",
        "edit",
        "patch",
    ];
    if EDIT.contains(&t.as_str()) {
        return ToolKind::Edit;
    }
    // Unknown → conservative: treat as exec so it is never auto-approved by
    // AcceptEdits.
    ToolKind::Exec
}

/// Wire/display label for a tool's risk class (see [`tool_kind`]).
pub(crate) fn tool_risk(tool: &str) -> &'static str {
    match tool_kind(tool) {
        ToolKind::ReadOnly => "read_only",
        ToolKind::Edit => "edit",
        ToolKind::Exec => "exec",
    }
}

/// A single declarative permission rule from `[[permission.rules]]` in
/// `.smedja/workspace.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct PermissionRule {
    /// Tool name or glob pattern (e.g. `"bash"`, `"write_*"`).
    pub tool: String,
    /// Glob matched against the `path` field of file-tool inputs.
    pub path_glob: Option<String>,
    /// Matched against the `command` field of bash inputs. A pattern WITHOUT a
    /// trailing `*` is an EXACT match (`cmd == pattern`); only a trailing `*`
    /// opts into prefix semantics (`"rm *"` covers `rm -rf …`). Exact-by-default
    /// keeps an allow-always on `git status` from also covering
    /// `git status && rm -rf ~`.
    pub command_pattern: Option<String>,
    /// Gate outcome when this rule matches.
    pub mode: RuleMode,
}

/// Gate outcome for a [`PermissionRule`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleMode {
    /// Always ask the user (same as cowork Ask mode).
    Ask,
    /// Let the call through without asking.
    Allow,
    /// Block the call before it reaches the gate.
    Deny,
}

/// Loads `[[permission.rules]]` from `.smedja/workspace.toml`, returning an
/// empty list if the file is absent or the section is missing.
///
/// A malformed file is logged loudly ([`tracing::error!`]) and fails closed to
/// an empty rule list — a corrupt rules file must never silently widen into
/// "no rules, mode decides"; it must also never panic the gate.
#[must_use]
pub fn load_permission_rules(workspace: &std::path::Path) -> Vec<PermissionRule> {
    match try_load_permission_rules(workspace) {
        Ok(rules) => rules,
        Err(e) => {
            tracing::error!(
                error = %e,
                workspace = %workspace.display(),
                "failed to parse .smedja/workspace.toml; permission rules disabled (fails closed to none)"
            );
            Vec::new()
        }
    }
}

/// The strict loader behind [`load_permission_rules`]: returns an error when
/// the file exists but cannot be read or parsed, so callers (and tests) can
/// distinguish "no rules" from "rules file is corrupt".
///
/// # Errors
///
/// Returns an [`std::io::Error`] on read failure (other than a missing file)
/// or when the TOML does not parse.
pub fn try_load_permission_rules(
    workspace: &std::path::Path,
) -> std::io::Result<Vec<PermissionRule>> {
    #[derive(Deserialize, Default)]
    struct WorkspaceToml {
        permission: Option<PermSection>,
    }
    #[derive(Deserialize, Default)]
    struct PermSection {
        rules: Option<Vec<PermissionRule>>,
    }
    let path = workspace.join(".smedja").join("workspace.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let parsed = toml::from_str::<WorkspaceToml>(&text).map_err(std::io::Error::other)?;
    Ok(parsed.permission.and_then(|p| p.rules).unwrap_or_default())
}

/// Cache of parsed permission rules keyed by the rules-file path, invalidated
/// on (mtime, len) change and on [`append_allow_rule`] writes. Rules are
/// consulted on every gated tool call; re-reading and re-parsing the TOML on
/// each call showed up in profiles of tool-heavy turns.
type RulesCacheEntry = (std::time::SystemTime, u64, Arc<Vec<PermissionRule>>);
static RULES_CACHE: std::sync::LazyLock<
    std::sync::Mutex<HashMap<std::path::PathBuf, RulesCacheEntry>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Drops any cached rules for the rules file at `path` (called after writes).
fn invalidate_rules_cache(path: &std::path::Path) {
    let mut cache = RULES_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.remove(path);
}

/// Loads and parses the workspace's permission rules, using [`RULES_CACHE`]
/// when the file's (mtime, len) is unchanged. Parse failures are logged via
/// [`load_permission_rules`]' loud path and cached as an empty list so a
/// corrupt file does not re-error (and re-log) on every gated call.
fn cached_permission_rules(workspace: &std::path::Path) -> Arc<Vec<PermissionRule>> {
    let path = workspace.join(".smedja").join("workspace.toml");
    let stat = std::fs::metadata(&path)
        .and_then(|m| m.modified().map(|mt| (mt, m.len())))
        .ok();
    {
        let cache = RULES_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let (Some((mtime, len)), Some((cached_mtime, cached_len, rules))) =
            (stat, cache.get(&path))
        {
            if mtime == *cached_mtime && len == *cached_len {
                return Arc::clone(rules);
            }
        }
    }
    let rules = Arc::new(load_permission_rules(workspace));
    if let Some((mtime, len)) = stat {
        let mut cache = RULES_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(path, (mtime, len, Arc::clone(&rules)));
    }
    rules
}

/// The ONE entry point for declarative permission-rule evaluation: every gate
/// path (native tool loop, ACP bridge, @shell fragments, the modify re-check)
/// consults this instead of open-coding `load_permission_rules` +
/// `evaluate_permission_rules`, so all of them share the parse cache and the
/// loud-on-corrupt loading behaviour. Returns `None` when no rule matches
/// (fall through to the session permission mode).
#[must_use]
pub fn evaluate_workspace_rules(
    workspace: &std::path::Path,
    tool: &str,
    args: &serde_json::Value,
) -> Option<PermissionDecision> {
    evaluate_permission_rules(&cached_permission_rules(workspace), tool, args)
}

/// Re-checks a *modify* decision's replacement input against the workspace's
/// permission rules: a human-approved rewrite must not smuggle a call a Deny
/// rule blocks (the original call was gated, the modified one was not).
/// Returns the deny reason when the modified call is rule-denied; `None` when
/// it parses and no Deny rule matches (or the instruction is not JSON — that
/// case is rejected upstream by [`CoworkGate::modify`]).
#[must_use]
pub fn modified_input_denied(
    workspace: &std::path::Path,
    tool: &str,
    new_input: &str,
) -> Option<String> {
    let new_args: serde_json::Value = serde_json::from_str(new_input).ok()?;
    if matches!(
        evaluate_workspace_rules(workspace, tool, &new_args),
        Some(PermissionDecision::Deny)
    ) {
        return Some(format!(
            "modified call blocked by permission rule for {tool}"
        ));
    }
    None
}

/// Appends an `Allow` [`PermissionRule`] to `.smedja/workspace.toml`, scoped to
/// `tool` and — when the call carries them — the specific `path`/`command` in
/// `args`. This is the backend-independent persistence behind an *allow-always*
/// decision: the rule is read by [`load_permission_rules`] and honoured by
/// [`evaluate_permission_rules`] on every future turn, whichever runner drives
/// it.
///
/// The rule is appended as a fresh `[[permission.rules]]` array-of-tables block
/// at end of file rather than round-tripping the whole document, so existing
/// hand-written content (comments, other sections) is preserved verbatim. TOML
/// merges every `[[permission.rules]]` block in document order into one array,
/// so an appended block is picked up alongside the pre-existing rules. The
/// block is built by hand (never via a `{ permission: … }` wrapper struct) so
/// no `[permission]` table header is emitted — the array-of-tables header
/// alone keeps repeated appends parseable regardless of serializer behaviour.
///
/// `*`, `?` and `\` in the persisted `path_glob` are escaped so the rule
/// matches the literal approved path and cannot widen into a wildcard on
/// future evaluation. Identical allow rules are not appended twice, and a
/// per-workspace write lock serialises concurrent appends.
///
/// # Errors
///
/// Returns an [`std::io::Error`] if the `.smedja` directory cannot be created or
/// the file cannot be written.
pub fn append_allow_rule(
    workspace: &std::path::Path,
    tool: &str,
    args: &serde_json::Value,
) -> std::io::Result<()> {
    // Scope the rule to the concrete target when the args expose one, so an
    // allow-always is as narrow as the call the user actually approved.
    let path_glob = args
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(escape_perm_glob);
    let command_pattern = args
        .get("command")
        .or_else(|| args.get("cmd"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let dir = workspace.join(".smedja");
    let path = dir.join("workspace.toml");
    let lock = rule_write_lock(&path);
    let _guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Dedupe: an identical allow rule adds no new scope.
    let existing = try_load_permission_rules(workspace).unwrap_or_default();
    let is_dup = existing.iter().any(|r| {
        r.tool == tool
            && r.mode == RuleMode::Allow
            && r.path_glob == path_glob
            && r.command_pattern == command_pattern
    });
    if is_dup {
        return Ok(());
    }

    let mut block = String::from("[[permission.rules]]\n");
    block.push_str(&format!(
        "tool = {}\n",
        toml::Value::String(tool.to_owned())
    ));
    if let Some(glob) = &path_glob {
        block.push_str(&format!(
            "path_glob = {}\n",
            toml::Value::String(glob.clone())
        ));
    }
    if let Some(pat) = &command_pattern {
        block.push_str(&format!(
            "command_pattern = {}\n",
            toml::Value::String(pat.clone())
        ));
    }
    block.push_str("mode = \"allow\"\n");

    std::fs::create_dir_all(&dir)?;
    let existing_text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out = existing_text;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&block);
    std::fs::write(&path, out)?;
    invalidate_rules_cache(&path);
    Ok(())
}

/// Per-rules-file write lock so concurrent allow-always resolutions (e.g. two
/// ACP approvals landing together) cannot interleave a read-modify-write.
fn rule_write_lock(path: &std::path::Path) -> Arc<std::sync::Mutex<()>> {
    static LOCKS: std::sync::LazyLock<
        std::sync::Mutex<HashMap<std::path::PathBuf, Arc<std::sync::Mutex<()>>>>,
    > = std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
    let mut map = LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(
        map.entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(std::sync::Mutex::new(()))),
    )
}

/// Escapes the glob metacharacters in a path so a persisted `path_glob`
/// matches the literal path only: `\` → `\\`, `*` → `\*`, `?` → `\?`.
fn escape_perm_glob(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '\\' | '*' | '?') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Evaluates `rules` in order; returns the first matching rule's
/// [`PermissionDecision`], or `None` if no rule matches (fall through to
/// session mode).
#[must_use]
pub fn evaluate_permission_rules(
    rules: &[PermissionRule],
    tool: &str,
    args: &serde_json::Value,
) -> Option<PermissionDecision> {
    for rule in rules {
        if !perm_glob_match(&rule.tool, tool) {
            continue;
        }
        if let Some(ref glob) = rule.path_glob {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if !perm_glob_match(glob, path) {
                continue;
            }
        }
        if let Some(ref pat) = rule.command_pattern {
            let cmd = args
                .get("command")
                .or_else(|| args.get("cmd"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // Exact match by default; only a trailing `*` opts into prefix
            // semantics (see [`PermissionRule::command_pattern`]).
            let matches = if let Some(prefix) = pat.strip_suffix('*') {
                cmd.starts_with(prefix)
            } else {
                cmd == pat.as_str()
            };
            if !matches {
                continue;
            }
        }
        return Some(match rule.mode {
            RuleMode::Ask => PermissionDecision::Ask,
            RuleMode::Allow => PermissionDecision::Allow,
            RuleMode::Deny => PermissionDecision::Deny,
        });
    }
    None
}

/// Minimal path-aware glob: `*` matches any sequence within one path segment
/// (it does NOT cross `/`), `**` crosses `/`, `?` matches exactly one
/// non-separator character, and `\x` matches a literal `x` (so persisted rules
/// can pin a path that itself contains glob metacharacters). Operates on
/// bytes; both patterns and values here are path-ish ASCII in practice.
fn perm_glob_match(pattern: &str, value: &str) -> bool {
    glob_match_bytes(pattern.as_bytes(), value.as_bytes())
}

fn glob_match_bytes(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'\\') if p.len() >= 2 => {
            // `\x` matches a literal `x` — no special meaning.
            s.first() == Some(&p[1]) && glob_match_bytes(&p[2..], &s[1..])
        }
        Some(b'*') if p.get(1) == Some(&b'*') => {
            // `**` crosses path separators (including zero characters).
            let rest = &p[2..];
            if rest.is_empty() {
                return true;
            }
            (0..=s.len()).any(|i| glob_match_bytes(rest, &s[i..]))
        }
        Some(b'*') => {
            // `*` stays within one path segment: never consumes a `/`.
            let rest = &p[1..];
            let mut i = 0;
            loop {
                if glob_match_bytes(rest, &s[i..]) {
                    return true;
                }
                if i >= s.len() || s[i] == b'/' {
                    return false;
                }
                i += 1;
            }
        }
        Some(b'?') => {
            // Exactly one non-separator character.
            match s.first() {
                Some(b'/') | None => false,
                Some(_) => glob_match_bytes(&p[1..], &s[1..]),
            }
        }
        Some(a) => s.first() == Some(a) && glob_match_bytes(&p[1..], &s[1..]),
    }
}

/// Evaluates the permission decision for a tool call under `mode`. Pure; the
/// blocking/asking happens in [`gate_tool`].
#[must_use]
pub fn evaluate(mode: PermissionMode, tool: &str) -> PermissionDecision {
    match (mode, tool_kind(tool)) {
        (_, ToolKind::ReadOnly)
        | (PermissionMode::Auto, _)
        | (PermissionMode::AcceptEdits, ToolKind::Edit) => PermissionDecision::Allow,
        (PermissionMode::Plan, _) => PermissionDecision::Deny,
        (PermissionMode::AcceptEdits, ToolKind::Exec) | (PermissionMode::Ask, _) => {
            PermissionDecision::Ask
        }
    }
}

impl CoworkGate {
    /// Submits a tool call for approval. Suspends until a decision arrives
    /// or the optional `timeout_secs` (0 = infinite) elapses.
    ///
    /// If `push` is `Some((dispatcher, turn_id))`, a [`TurnEvent::CoworkRequest`]
    /// is published immediately after registering the pending approval so the TUI
    /// receives the request via the NDJSON stream instead of polling.
    ///
    /// Returns [`Decision::Deny`] on timeout or channel close (fail-closed).
    pub async fn intercept(
        &self,
        prompt: ApprovalPrompt,
        timeout_secs: u64,
        push: Option<(&Dispatcher, Option<&str>)>,
    ) -> Decision {
        self.intercept_tracked(prompt, timeout_secs, push).await.1
    }

    /// Like [`Self::intercept`] but also returns the pending [`ApprovalId`] the
    /// gate assigned. The id is stable for the lifetime of the request and lets a
    /// caller correlate the resolution with out-of-band state — in particular the
    /// industry-ACP `session/request_permission` bridge uses it to consult
    /// [`Self::take_always`] and decide whether to persist an allow-always rule.
    ///
    /// [`Self::intercept`] simply discards the id, so all existing callers are
    /// unchanged.
    pub async fn intercept_tracked(
        &self,
        prompt: ApprovalPrompt,
        timeout_secs: u64,
        push: Option<(&Dispatcher, Option<&str>)>,
    ) -> (ApprovalId, Decision) {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(
                id.clone(),
                PendingApproval {
                    prompt: prompt.clone(),
                    tx,
                    notify: push.map(|(dispatcher, _)| dispatcher.clone()),
                },
            );
        }
        // Guarantee the pending entry is removed on every exit path below —
        // timeout, sender-dropped, or cancellation — not just via `resolve`.
        let _guard = PendingGuard {
            pending: Arc::clone(&self.pending),
            id: id.clone(),
        };
        if let Some((dispatcher, turn_id)) = push {
            dispatcher.publish(TurnEvent::CoworkRequest {
                approval_id: id.clone(),
                tool: prompt.tool.clone(),
                step_n: prompt.step_n,
                args_display: prompt.args_scrubbed.to_string(),
                reasoning: prompt.reasoning.clone(),
                cwd: prompt.cwd.clone(),
                turn_id: turn_id.map(str::to_owned),
                supports_modify: prompt.supports_modify,
                correlation: CorrelationCtx {
                    agent_name: prompt.agent.clone(),
                    ..CorrelationCtx::default()
                },
            });
        }
        tracing::info!(
            approval_id = %id,
            tool = %prompt.tool,
            step = prompt.step_n,
            "cowork gate: awaiting human decision",
        );

        let decision = if timeout_secs == 0 {
            // Wait indefinitely; deny if the channel closes unexpectedly.
            rx.await
                .unwrap_or_else(|_| Decision::Deny("channel closed".to_owned()))
        } else {
            match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await {
                Ok(Ok(decision)) => decision,
                Ok(Err(_)) => {
                    // Sender dropped without sending — deny.
                    Decision::Deny("channel closed".to_owned())
                }
                Err(_) => {
                    tracing::warn!(
                        approval_id = %id,
                        timeout_secs,
                        "cowork gate: approval timed out; denying",
                    );
                    // Remove the pending entry FIRST: a resolve() racing in
                    // after the broadcast must find nothing (the user already
                    // saw the prompt time out — resolving it now would execute
                    // a call nobody approved in time).
                    let removed = self.pending.lock().await.remove(&id);
                    let dispatcher = removed
                        .and_then(|entry| entry.notify)
                        .or_else(|| push.map(|(d, _)| d.clone()));
                    if let Some(dispatcher) = dispatcher {
                        // Broadcast the resolution so every stream client (and the
                        // session buffer) drops the stale prompt. This is the only
                        // signal on turn_id-less paths (claude hook / ACP bridge):
                        // the AssistantDelta below is dropped there.
                        dispatcher.publish(TurnEvent::CoworkResolved {
                            approval_id: id.clone(),
                            outcome: smedja_bellows::CoworkOutcome::Timeout,
                        });
                        if let Some((_, turn_id)) = push {
                            // Surface the timeout on the session stream so the user
                            // sees the turn resume (denied) instead of a silently
                            // hung popup. Turn-scoped paths only — turn-less
                            // streams drop deltas.
                            dispatcher.publish(TurnEvent::AssistantDelta {
                                content: format!(
                                    "\n[cowork] approval for `{}` timed out after {timeout_secs}s — denied\n",
                                    prompt.tool
                                ),
                                turn_id: turn_id.map(str::to_owned),
                                correlation: CorrelationCtx::default(),
                            });
                        }
                    }
                    Decision::Deny("timeout".to_owned())
                }
            }
        };
        (id, decision)
    }

    /// Resolves a pending approval with [`Decision::Approve`].
    ///
    /// Returns `true` if the approval ID was found and resolved.
    pub async fn approve(&self, id: &str) -> bool {
        self.resolve(id, Decision::Approve).await
    }

    /// Resolves a pending approval with [`Decision::Approve`] **and** marks it as
    /// an *allow-always* resolution: the user chose to allow this class of call
    /// for the rest of the workspace, not just this once.
    ///
    /// The extra scope is recorded out-of-band (a set of approval ids) rather
    /// than on the [`Decision`] enum, so the native tool loop is untouched. The
    /// resolution site consumes it via [`Self::take_always`] to persist a
    /// `[[permission.rules]]` Allow entry. The flag is inserted *before*
    /// resolving so it is guaranteed visible when the suspended
    /// [`Self::intercept_tracked`] wakes.
    ///
    /// Returns `(found, note)`: `found` is `true` when the approval ID was
    /// pending and is now resolved; `note` carries the persistence veto when
    /// [`allow_always_skip_reason`] already rules out persisting a rule for
    /// this prompt (computed from the scrubbed args before resolution, so an
    /// RPC responder can surface it immediately). The RAW-args veto in the
    /// suspend path remains the authoritative backstop — scrubbing can hide a
    /// bare-token `path`/`command` the raw check would still catch.
    pub async fn approve_always(&self, id: &str) -> (bool, Option<String>) {
        let note = {
            let pending = self.pending.lock().await;
            pending
                .get(id)
                .and_then(|e| allow_always_skip_reason(&e.prompt.args_scrubbed))
                .map(str::to_owned)
        };
        self.always_ids.lock().await.insert(id.to_owned());
        let found = self.resolve(id, Decision::Approve).await;
        if found {
            if let Some(note) = note {
                self.always_notes
                    .lock()
                    .await
                    .insert(id.to_owned(), note.clone());
                return (true, Some(note));
            }
        } else {
            // The id was unknown (already resolved / timed out): don't leak a
            // dangling always-flag.
            self.always_ids.lock().await.remove(id);
        }
        (found, None)
    }

    /// Consumes and returns whether `id` was resolved with an allow-always scope
    /// (see [`Self::approve_always`]). Idempotent: a second call returns `false`.
    pub async fn take_always(&self, id: &str) -> bool {
        self.always_ids.lock().await.remove(id)
    }

    /// Consumes the persistence-veto note [`Self::approve_always`] captured for
    /// `id`, if any. Idempotent like [`Self::take_always`].
    pub async fn take_always_note(&self, id: &str) -> Option<String> {
        self.always_notes.lock().await.remove(id)
    }

    /// Resolves a pending approval with [`Decision::Deny`].
    ///
    /// Returns `true` if the approval ID was found and resolved.
    pub async fn deny(&self, id: &str, reason: String) -> bool {
        self.resolve(id, Decision::Deny(reason)).await
    }

    /// Resolves a pending approval with [`Decision::Modify`] — the instruction
    /// REPLACES the tool input, so it must be a JSON object of replacement
    /// arguments.
    ///
    /// Validation happens here, before resolution, so a bad modify answers the
    /// caller with a [`ModifyRejection`] it can surface to the user instead of
    /// silently degrading to a deny; the prompt stays pending for a corrected
    /// decision. Backends without a modify channel reject with
    /// [`ModifyRejection::Unsupported`].
    pub async fn modify(&self, id: &str, instruction: String) -> Result<(), ModifyRejection> {
        {
            let pending = self.pending.lock().await;
            let Some(entry) = pending.get(id) else {
                return Err(ModifyRejection::UnknownId);
            };
            if !entry.prompt.supports_modify {
                return Err(ModifyRejection::Unsupported {
                    tool: entry.prompt.tool.clone(),
                });
            }
            match serde_json::from_str::<serde_json::Value>(&instruction) {
                Ok(v) if v.is_object() => {}
                Ok(_) => {
                    return Err(ModifyRejection::InvalidInstruction(
                        "expected a JSON object, got another JSON value".to_owned(),
                    ));
                }
                Err(e) => {
                    return Err(ModifyRejection::InvalidInstruction(e.to_string()));
                }
            }
        }
        if self.resolve(id, Decision::Modify(instruction)).await {
            Ok(())
        } else {
            Err(ModifyRejection::UnknownId)
        }
    }

    /// Lists pending approvals with their full prompts, ordered by insertion UUID
    /// (arbitrary but stable within a poll interval).
    pub async fn list_pending(&self) -> Vec<(ApprovalId, ApprovalPrompt)> {
        self.pending
            .lock()
            .await
            .iter()
            .map(|(id, p)| (id.clone(), p.prompt.clone()))
            .collect()
    }

    /// Gates a single tool call under the gate's current [`PermissionMode`]:
    /// persisted `[[permission.rules]]` first (when `ctx.workspace` is known),
    /// then allow/deny outright per [`evaluate`], or — for `Ask` — suspend on
    /// the gate (≤30 min) until the user decides. Returns the resolved
    /// [`GateOutcome`].
    ///
    /// `args` are the RAW tool args (execution keeps its own copy upstream);
    /// the popup/stream copy is scrubbed here. An allow-always resolution
    /// persists a matching Allow rule via [`append_allow_rule`], whichever
    /// backend path drove the approval — unless [`allow_always_skip_reason`]
    /// vetoes it (nothing to scope to, or a secret in the payload), in which
    /// case the outcome's `note` says why.
    ///
    /// Pass `push` to have a [`TurnEvent::CoworkRequest`] pushed via the NDJSON
    /// stream so the TUI receives it without polling.
    pub async fn gate_tool(
        &self,
        step_n: u32,
        tool: &str,
        args: serde_json::Value,
        reasoning: &str,
        ctx: &GateContext,
        push: Option<(&Dispatcher, Option<&str>)>,
    ) -> GateOutcome {
        match self.rule_decision(ctx, tool, &args) {
            Some(PermissionDecision::Allow) => {
                return GateOutcome {
                    decision: Decision::Approve,
                    note: None,
                };
            }
            Some(PermissionDecision::Deny) => {
                return GateOutcome {
                    decision: Decision::Deny(format!("blocked by permission rule for {tool}")),
                    note: None,
                };
            }
            // An explicit `Ask` rule forces the interactive gate even when the
            // session mode (AcceptEdits/Auto) would auto-approve.
            Some(PermissionDecision::Ask) => {
                let (decision, note) = self
                    .suspend(step_n, tool, &args, reasoning, ctx, push)
                    .await;
                return GateOutcome { decision, note };
            }
            None => {}
        }
        let mode = self.mode().await;
        match evaluate(mode, tool) {
            PermissionDecision::Allow => GateOutcome {
                decision: Decision::Approve,
                note: None,
            },
            PermissionDecision::Deny => GateOutcome {
                decision: Decision::Deny(format!("blocked by {} mode", mode.as_str())),
                note: None,
            },
            PermissionDecision::Ask => {
                let (decision, note) = self
                    .suspend(step_n, tool, &args, reasoning, ctx, push)
                    .await;
                GateOutcome { decision, note }
            }
        }
    }

    /// Like [`Self::gate_tool`] but always suspends for a human decision,
    /// ignoring the mode's allow/auto — for high-risk roles (`IaC`) whose
    /// mutations must be confirmed even under `AcceptEdits`/`Auto`. Persisted
    /// Allow rules still short-circuit: an explicit allow-always is a human
    /// decision too.
    pub async fn gate_tool_forced_ask(
        &self,
        step_n: u32,
        tool: &str,
        args: serde_json::Value,
        reasoning: &str,
        ctx: &GateContext,
        push: Option<(&Dispatcher, Option<&str>)>,
    ) -> GateOutcome {
        match self.rule_decision(ctx, tool, &args) {
            Some(PermissionDecision::Allow) => {
                return GateOutcome {
                    decision: Decision::Approve,
                    note: None,
                };
            }
            Some(PermissionDecision::Deny) => {
                return GateOutcome {
                    decision: Decision::Deny(format!("blocked by permission rule for {tool}")),
                    note: None,
                };
            }
            Some(PermissionDecision::Ask) | None => {}
        }
        let (decision, note) = self
            .suspend(step_n, tool, &args, reasoning, ctx, push)
            .await;
        GateOutcome { decision, note }
    }

    /// Evaluates persisted `[[permission.rules]]` for this call. Returns `None`
    /// when no workspace is known or no rule matches (fall through to the
    /// session mode); an `Ask` rule surfaces as [`PermissionDecision::Ask`] so
    /// the caller can force the interactive gate.
    fn rule_decision(
        &self,
        ctx: &GateContext,
        tool: &str,
        args: &serde_json::Value,
    ) -> Option<PermissionDecision> {
        let workspace = ctx.workspace.as_deref()?;
        evaluate_workspace_rules(workspace, tool, args)
    }

    /// Shared suspend-and-persist tail of [`Self::gate_tool`] and
    /// [`Self::gate_tool_forced_ask`]: builds the scrubbed prompt, waits on the
    /// gate, then persists an allow-always rule when the resolution carried
    /// that scope. Returns the decision plus a note when the rule was NOT
    /// persisted ([`allow_always_skip_reason`]), so the caller can tell the
    /// user the approval applies once only.
    async fn suspend(
        &self,
        step_n: u32,
        tool: &str,
        args: &serde_json::Value,
        reasoning: &str,
        ctx: &GateContext,
        push: Option<(&Dispatcher, Option<&str>)>,
    ) -> (Decision, Option<String>) {
        let mut prompt = ApprovalPrompt::new(step_n, tool, args, reasoning);
        prompt.agent.clone_from(&ctx.agent);
        prompt.cwd = ctx.workspace.as_ref().map(|w| w.display().to_string());
        prompt.supports_modify = ctx.supports_modify;
        let (id, decision) = self
            .intercept_tracked(prompt, APPROVAL_TIMEOUT_SECS, push)
            .await;
        // Allow-always persists a `[[permission.rules]]` Allow entry so the
        // choice sticks across turns and backends — unless the payload is
        // unscopeable (tool-only), globby, or carries a secret, in which case
        // the single approval stands and the note explains why. The RAW-args
        // veto here is authoritative for persistence; the note captured by
        // `approve_always` (from the scrubbed args) is the fallback.
        let mut note = None;
        if matches!(decision, Decision::Approve) && self.take_always(&id).await {
            let veto = allow_always_skip_reason(args)
                .map(str::to_owned)
                .or(self.take_always_note(&id).await);
            if let Some(reason) = veto {
                tracing::info!(tool = %tool, "allow-always rule not persisted: {reason}");
                note = Some(reason);
            } else if let Some(workspace) = ctx.workspace.as_deref() {
                if let Err(e) = append_allow_rule(workspace, tool, args) {
                    tracing::warn!(error = %e, tool = %tool, "failed to persist allow-always rule");
                }
            }
        }
        (decision, note)
    }

    /// The gate's current permission mode.
    pub async fn mode(&self) -> PermissionMode {
        *self.mode.lock().await
    }

    /// Sets the permission mode; returns the new value.
    pub async fn set_mode(&self, mode: PermissionMode) -> PermissionMode {
        *self.mode.lock().await = mode;
        mode
    }

    /// Cycles to the next permission mode (Shift+Tab); returns the new value.
    pub async fn cycle_mode(&self) -> PermissionMode {
        let mut m = self.mode.lock().await;
        *m = m.next();
        *m
    }

    async fn resolve(&self, id: &str, decision: Decision) -> bool {
        let mut pending = self.pending.lock().await;
        if let Some(entry) = pending.remove(id) {
            // Broadcast the resolution on the same dispatcher the request went
            // out on, so live streams dismiss the overlay and the session
            // buffer drops the stale request (a modify still runs the call,
            // so it reports as approved).
            if let Some(dispatcher) = &entry.notify {
                let outcome = match &decision {
                    Decision::Approve | Decision::Modify(_) => {
                        smedja_bellows::CoworkOutcome::Approved
                    }
                    Decision::Deny(_) => smedja_bellows::CoworkOutcome::Denied,
                };
                dispatcher.publish(TurnEvent::CoworkResolved {
                    approval_id: id.to_owned(),
                    outcome,
                });
            }
            let _ = entry.tx.send(decision);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn prompt() -> ApprovalPrompt {
        let mut p = ApprovalPrompt::new(1, "bash", &json!({"cmd": "ls"}), "list files");
        p.plan_summary = "exploration".into();
        p
    }

    #[test]
    fn evaluate_policy_matrix() {
        // Read-only is always allowed, regardless of mode.
        assert_eq!(
            evaluate(PermissionMode::Ask, "read_file"),
            PermissionDecision::Allow
        );
        assert_eq!(
            evaluate(PermissionMode::Plan, "graph_query"),
            PermissionDecision::Allow
        );
        // Auto allows everything.
        assert_eq!(
            evaluate(PermissionMode::Auto, "bash"),
            PermissionDecision::Allow
        );
        assert_eq!(
            evaluate(PermissionMode::Auto, "write_file"),
            PermissionDecision::Allow
        );
        // Plan denies every mutation (read-only mode).
        assert_eq!(
            evaluate(PermissionMode::Plan, "write_file"),
            PermissionDecision::Deny
        );
        assert_eq!(
            evaluate(PermissionMode::Plan, "exec_bash"),
            PermissionDecision::Deny
        );
        // Ask asks on every mutation.
        assert_eq!(
            evaluate(PermissionMode::Ask, "write_file"),
            PermissionDecision::Ask
        );
        assert_eq!(
            evaluate(PermissionMode::Ask, "bash"),
            PermissionDecision::Ask
        );
        // AcceptEdits: known edits auto-allow; shell + unknown still ask.
        assert_eq!(
            evaluate(PermissionMode::AcceptEdits, "edit_file"),
            PermissionDecision::Allow
        );
        assert_eq!(
            evaluate(PermissionMode::AcceptEdits, "write_file"),
            PermissionDecision::Allow
        );
        assert_eq!(
            evaluate(PermissionMode::AcceptEdits, "bash"),
            PermissionDecision::Ask
        );
        assert_eq!(
            evaluate(PermissionMode::AcceptEdits, "mystery_tool"),
            PermissionDecision::Ask
        );
    }

    #[test]
    fn lsp_tools_classified_as_read_or_edit() {
        // Read-only lsp tools are never gated.
        for t in [
            "lsp_definition",
            "lsp_references",
            "lsp_hover",
            "lsp_document_symbols",
            "lsp_workspace_symbols",
        ] {
            assert_eq!(super::tool_kind(t), super::ToolKind::ReadOnly, "{t}");
            assert_eq!(evaluate(PermissionMode::Ask, t), PermissionDecision::Allow);
        }
        // Rename is a mutation: auto-approved only under AcceptEdits/Auto, asked
        // under Ask, denied under Plan.
        assert_eq!(super::tool_kind("lsp_rename_symbol"), super::ToolKind::Edit);
        assert_eq!(
            evaluate(PermissionMode::AcceptEdits, "lsp_rename_symbol"),
            PermissionDecision::Allow
        );
        assert_eq!(
            evaluate(PermissionMode::Ask, "lsp_rename_symbol"),
            PermissionDecision::Ask
        );
        assert_eq!(
            evaluate(PermissionMode::Plan, "lsp_rename_symbol"),
            PermissionDecision::Deny
        );
    }

    #[test]
    fn permission_mode_roundtrip_and_cycle() {
        for m in [
            PermissionMode::Ask,
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
            PermissionMode::Auto,
        ] {
            assert_eq!(PermissionMode::parse_lenient(m.as_str()), m);
        }
        assert_eq!(
            PermissionMode::parse_lenient("garbage"),
            PermissionMode::Ask
        );
        assert_eq!(
            PermissionMode::parse_lenient("accept-edits"),
            PermissionMode::AcceptEdits
        );
        // Full Shift+Tab cycle returns to start.
        assert_eq!(
            PermissionMode::Ask.next().next().next().next(),
            PermissionMode::Ask
        );
    }

    #[tokio::test]
    async fn gate_tool_allow_deny_and_ask_paths() {
        let gate = CoworkGate::default(); // Ask mode by default.
        let ctx = GateContext::default();
        // Read-only: allowed, no pending entry.
        assert!(matches!(
            gate.gate_tool(1, "read_file", json!({}), "", &ctx, None)
                .await
                .decision,
            Decision::Approve
        ));
        assert!(gate.list_pending().await.is_empty());

        // Plan mode denies a write outright.
        gate.set_mode(PermissionMode::Plan).await;
        assert!(matches!(
            gate.gate_tool(1, "write_file", json!({}), "", &ctx, None)
                .await
                .decision,
            Decision::Deny(_)
        ));

        // Ask mode suspends; approving concurrently resolves it.
        let gate = Arc::new(CoworkGate::default());
        let g2 = Arc::clone(&gate);
        let h = tokio::spawn(async move {
            g2.gate_tool(1, "write_file", json!({ "path": "x" }), "edit", &ctx, None)
                .await
        });
        let id = {
            let mut found = None;
            for _ in 0..1000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("pending approval should appear")
        };
        assert!(gate.approve(&id).await);
        assert!(matches!(h.await.unwrap().decision, Decision::Approve));
    }

    #[tokio::test]
    async fn approve_resolves_pending() {
        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);

        let handle = tokio::spawn(async move { gate2.intercept(prompt(), 0, None).await });

        // Give the intercept task time to register itself.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let pending = gate.list_pending().await;
        assert_eq!(pending.len(), 1);
        let id = pending[0].0.clone();

        assert!(gate.approve(&id).await);
        let decision = handle.await.unwrap();
        assert!(matches!(decision, Decision::Approve));
    }

    #[tokio::test]
    async fn deny_resolves_with_reason() {
        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);

        let handle = tokio::spawn(async move { gate2.intercept(prompt(), 0, None).await });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let pending = gate.list_pending().await;
        let id = pending[0].0.clone();

        assert!(gate.deny(&id, "too risky".into()).await);
        let decision = handle.await.unwrap();
        assert!(matches!(decision, Decision::Deny(r) if r == "too risky"));
    }

    #[tokio::test]
    async fn timeout_denies() {
        let gate = CoworkGate::default();
        let decision = gate.intercept(prompt(), 1, None).await;
        assert!(matches!(decision, Decision::Deny(r) if r == "timeout"));
    }

    #[tokio::test]
    async fn timeout_removes_pending_entry() {
        // A timed-out prompt (walk-away) must not leak an entry in the pending
        // map: the drop-guard removes it on the timeout exit path.
        let gate = CoworkGate::default();
        let decision = gate.intercept(prompt(), 1, None).await;
        assert!(matches!(decision, Decision::Deny(r) if r == "timeout"));
        assert!(
            gate.list_pending().await.is_empty(),
            "timed-out approval must not leak a pending entry"
        );
    }

    #[tokio::test]
    async fn unknown_id_resolve_returns_false() {
        let gate = CoworkGate::default();
        assert!(!gate.approve("nonexistent-id").await);
        assert!(!gate.deny("nonexistent-id", "reason".into()).await);
        assert!(matches!(
            gate.modify("nonexistent-id", "instruction".into()).await,
            Err(super::ModifyRejection::UnknownId)
        ));
    }

    /// Session-skip path: when a Codex-backed session calls intercept but the
    /// caller is responsible for skipping intercept entirely, the gate itself
    /// still works correctly — approve resolves immediately.
    #[tokio::test]
    async fn session_skip_approve_resolves() {
        // Callers that want to skip the gate simply don't call intercept.
        // This test exercises that the gate resolves correctly when used directly,
        // which is all we can assert from outside the call site.
        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);

        let handle = tokio::spawn(async move { gate2.intercept(prompt(), 0, None).await });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let pending = gate.list_pending().await;
        assert_eq!(pending.len(), 1, "one pending approval expected");
        let id = pending[0].0.clone();
        gate.approve(&id).await;

        let decision = handle.await.unwrap();
        assert!(matches!(decision, Decision::Approve));
        assert!(gate.list_pending().await.is_empty());
    }

    #[tokio::test]
    async fn approval_round_trip_emits_pending_then_resolves() {
        let gate = Arc::new(CoworkGate::default());
        let gate_ref = Arc::clone(&gate);

        // Spawn a task that intercepts a tool call.
        let intercept_handle =
            tokio::spawn(async move { gate_ref.intercept(prompt(), 5, None).await });

        // Give intercept time to register the pending entry.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Verify the pending entry is visible.
        let pending = gate.list_pending().await;
        assert_eq!(pending.len(), 1, "expected one pending approval");
        let id = pending[0].0.clone();

        // Approve it.
        let resolved = gate.approve(&id).await;
        assert!(resolved, "approve must return true for a known id");

        // The intercepting task should now resolve to Approve.
        let decision = intercept_handle.await.expect("intercept task panicked");
        assert!(
            matches!(decision, Decision::Approve),
            "expected Decision::Approve after approval"
        );
    }

    #[tokio::test]
    async fn intercept_emits_pending_for_any_runner() {
        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);

        let handle = tokio::spawn(async move { gate2.intercept(prompt(), 0, None).await });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let pending = gate.list_pending().await;
        assert_eq!(
            pending.len(),
            1,
            "intercept must create a pending entry for any runner"
        );
        assert_eq!(pending[0].1.tool, "bash");

        // Clean up: approve so the spawned task can finish.
        let id = pending[0].0.clone();
        gate.approve(&id).await;
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn intercept_push_publishes_cowork_request_event() {
        use smedja_bellows::Dispatcher;

        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);
        let dispatcher = Arc::new(Dispatcher::new(16));
        let mut rx = dispatcher.subscribe();
        let disp_ref = Arc::clone(&dispatcher);

        let handle = tokio::spawn(async move {
            gate2
                .intercept(prompt(), 0, Some((disp_ref.as_ref(), Some("t-99"))))
                .await
        });

        // The CoworkRequest event must arrive before the gate suspends.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let event = rx.try_recv().expect("CoworkRequest must be published");
        let smedja_bellows::TurnEvent::CoworkRequest {
            ref tool,
            ref turn_id,
            ..
        } = event
        else {
            panic!("expected CoworkRequest, got {event:?}");
        };
        assert_eq!(tool, "bash");
        assert_eq!(turn_id.as_deref(), Some("t-99"));

        // Clean up.
        let pending = gate.list_pending().await;
        gate.approve(&pending[0].0).await;
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn intercept_push_carries_cwd_from_gate_context() {
        use smedja_bellows::Dispatcher;

        // The suspend path copies GateContext.workspace onto the prompt; the
        // published CoworkRequest must carry it as `cwd` so clients can show
        // where the gated call runs.
        let ws = tempfile::tempdir().unwrap();
        let gate = Arc::new(CoworkGate::default());
        let dispatcher = Arc::new(Dispatcher::new(16));
        let mut rx = dispatcher.subscribe();
        let ctx = super::GateContext {
            agent: None,
            workspace: Some(ws.path().to_path_buf()),
            supports_modify: true,
        };
        let g2 = Arc::clone(&gate);
        let d2 = Arc::clone(&dispatcher);
        let handle = tokio::spawn(async move {
            g2.gate_tool(
                1,
                "bash",
                json!({"command": "ls"}),
                "",
                &ctx,
                Some((d2.as_ref(), Some("t-cwd"))),
            )
            .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let event = rx.try_recv().expect("CoworkRequest must be published");
        let smedja_bellows::TurnEvent::CoworkRequest { ref cwd, .. } = event else {
            panic!("expected CoworkRequest, got {event:?}");
        };
        assert_eq!(
            cwd.as_deref(),
            Some(ws.path().display().to_string().as_str())
        );

        let pending = gate.list_pending().await;
        gate.approve(&pending[0].0).await;
        handle.await.unwrap();
    }

    // ── permission rules ──────────────────────────────────────────────────────
    #[test]
    fn permission_rules_deny_blocks() {
        let rules = vec![super::PermissionRule {
            tool: "bash".into(),
            path_glob: None,
            command_pattern: None,
            mode: super::RuleMode::Deny,
        }];
        let result = super::evaluate_permission_rules(&rules, "bash", &serde_json::Value::Null);
        assert_eq!(result, Some(super::PermissionDecision::Deny));
    }

    #[test]
    fn permission_rules_allow_bypasses_gate() {
        let rules = vec![super::PermissionRule {
            tool: "read_file".into(),
            path_glob: Some("src/**".into()),
            command_pattern: None,
            mode: super::RuleMode::Allow,
        }];
        let args = json!({"path": "src/main.rs"});
        let result = super::evaluate_permission_rules(&rules, "read_file", &args);
        assert_eq!(result, Some(super::PermissionDecision::Allow));
    }

    #[test]
    fn permission_rules_fallthrough_when_no_match() {
        let rules = vec![super::PermissionRule {
            tool: "write_file".into(),
            path_glob: None,
            command_pattern: None,
            mode: super::RuleMode::Deny,
        }];
        let result =
            super::evaluate_permission_rules(&rules, "read_file", &serde_json::Value::Null);
        assert_eq!(
            result, None,
            "non-matching rule must not produce a decision"
        );
    }

    #[test]
    fn permission_rules_path_glob_non_match_skips_rule() {
        let rules = vec![super::PermissionRule {
            tool: "write_file".into(),
            path_glob: Some("src/**".into()),
            command_pattern: None,
            mode: super::RuleMode::Deny,
        }];
        // path is outside src/ — rule must not match
        let args = json!({"path": "tests/foo.rs"});
        let result = super::evaluate_permission_rules(&rules, "write_file", &args);
        assert_eq!(result, None, "path outside glob must not trigger rule");
    }

    // ── allow-always scope + rule persistence ────────────────────────────────

    #[tokio::test]
    async fn approve_always_flags_id_then_take_always_consumes_it() {
        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);
        let handle = tokio::spawn(async move { gate2.intercept_tracked(prompt(), 0, None).await });

        // Wait for the pending entry to register.
        let id = {
            let mut found = None;
            for _ in 0..1000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("pending approval should appear")
        };

        let (found, note) = gate.approve_always(&id).await;
        assert!(found, "approve_always must resolve");
        assert!(note.is_none(), "a scopeable command carries no veto note");
        let (returned_id, decision) = handle.await.unwrap();
        assert_eq!(returned_id, id, "intercept_tracked must return the gate id");
        assert!(matches!(decision, Decision::Approve));
        // The always-scope flag is readable exactly once (consumed).
        assert!(
            gate.take_always(&id).await,
            "id must carry allow-always scope"
        );
        assert!(
            !gate.take_always(&id).await,
            "take_always must be idempotent (consumed)"
        );
    }

    #[tokio::test]
    async fn approve_always_unknown_id_leaves_no_dangling_flag() {
        let gate = CoworkGate::default();
        assert!(!gate.approve_always("no-such-id").await.0);
        assert!(
            !gate.take_always("no-such-id").await,
            "an unresolved allow-always must not leak a flag"
        );
    }

    #[test]
    fn append_allow_rule_writes_readable_rule() {
        let ws = tempfile::tempdir().unwrap();
        super::append_allow_rule(ws.path(), "bash", &json!({"command": "git status"})).unwrap();

        // The persisted rule round-trips through the loader + evaluator.
        let rules = super::load_permission_rules(ws.path());
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].tool, "bash");
        assert_eq!(rules[0].mode, super::RuleMode::Allow);
        assert_eq!(
            super::evaluate_permission_rules(&rules, "bash", &json!({"command": "git status"})),
            Some(super::PermissionDecision::Allow),
            "the persisted allow-always rule must allow the same call on future turns"
        );
    }

    #[test]
    fn append_allow_rule_preserves_existing_content_and_appends() {
        let ws = tempfile::tempdir().unwrap();
        let dir = ws.path().join(".smedja");
        std::fs::create_dir_all(&dir).unwrap();
        // A hand-written file with a comment and an unrelated section.
        std::fs::write(
            dir.join("workspace.toml"),
            "# hand written\n[embedder]\nbackend = \"fnv\"\n",
        )
        .unwrap();

        super::append_allow_rule(ws.path(), "write_file", &json!({"path": "src/main.rs"})).unwrap();

        let content = std::fs::read_to_string(dir.join("workspace.toml")).unwrap();
        assert!(content.contains("# hand written"), "comment must survive");
        assert!(
            content.contains("[embedder]"),
            "existing section must survive"
        );
        assert!(content.contains("[[permission.rules]]"));

        // Both the pre-existing config and the appended rule parse.
        let rules = super::load_permission_rules(ws.path());
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].tool, "write_file");
        assert_eq!(rules[0].path_glob.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn append_allow_rule_twice_keeps_file_parseable() {
        let ws = tempfile::tempdir().unwrap();
        super::append_allow_rule(ws.path(), "bash", &json!({"command": "git status"})).unwrap();
        super::append_allow_rule(ws.path(), "write_file", &json!({"path": "src/main.rs"})).unwrap();

        let content = std::fs::read_to_string(ws.path().join(".smedja/workspace.toml")).unwrap();
        let rules = super::try_load_permission_rules(ws.path())
            .unwrap_or_else(|e| panic!("file after two appends must parse: {e}\n{content}"));
        assert_eq!(rules.len(), 2, "both appended rules must load");
        assert_eq!(rules[0].tool, "bash");
        assert_eq!(rules[1].tool, "write_file");
    }

    #[test]
    fn append_allow_rule_onto_existing_permission_table_keeps_parseable() {
        // A hand-written file that already declares the [permission] table.
        let ws = tempfile::tempdir().unwrap();
        let dir = ws.path().join(".smedja");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("workspace.toml"),
            "[permission]\n[[permission.rules]]\ntool = \"bash\"\ncommand_pattern = \"ls\"\nmode = \"allow\"\n",
        )
        .unwrap();

        super::append_allow_rule(ws.path(), "write_file", &json!({"path": "src/main.rs"})).unwrap();

        let rules = super::try_load_permission_rules(ws.path())
            .expect("append onto an existing [permission] table must still parse");
        assert_eq!(
            rules.len(),
            2,
            "hand-written and appended rules must both load"
        );
    }

    #[test]
    fn append_allow_rule_dedupes_identical_rule() {
        let ws = tempfile::tempdir().unwrap();
        super::append_allow_rule(ws.path(), "bash", &json!({"command": "git status"})).unwrap();
        super::append_allow_rule(ws.path(), "bash", &json!({"command": "git status"})).unwrap();
        let rules = super::load_permission_rules(ws.path());
        assert_eq!(
            rules.len(),
            1,
            "an identical allow rule must not be appended twice"
        );
    }

    #[test]
    fn load_permission_rules_logs_and_returns_empty_on_parse_error() {
        let ws = tempfile::tempdir().unwrap();
        let dir = ws.path().join(".smedja");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("workspace.toml"), "[[permission.rules]\nnot toml").unwrap();

        assert!(
            super::try_load_permission_rules(ws.path()).is_err(),
            "a malformed file must surface an error to the loud loader"
        );
        assert!(
            super::load_permission_rules(ws.path()).is_empty(),
            "the quiet loader still fails closed to no rules"
        );
    }

    #[test]
    fn perm_glob_single_star_stays_within_path_segment() {
        assert!(super::perm_glob_match("src/*.rs", "src/main.rs"));
        assert!(
            !super::perm_glob_match("src/*.rs", "src/nested/main.rs"),
            "single * must not cross '/'"
        );
        assert!(
            super::perm_glob_match("src/**", "src/nested/main.rs"),
            "** must cross '/'"
        );
        assert!(super::perm_glob_match("src/**/main.rs", "src/a/b/main.rs"));
        assert!(super::perm_glob_match("src/**", "src/main.rs"));
    }

    #[test]
    fn perm_glob_escape_matches_literal_specials() {
        assert!(super::perm_glob_match("src/\\*.rs", "src/*.rs"));
        assert!(!super::perm_glob_match("src/\\*.rs", "src/main.rs"));
        assert!(super::perm_glob_match("a\\?b", "a?b"));
        assert!(!super::perm_glob_match("a\\?b", "axb"));
        assert!(super::perm_glob_match("a\\\\b", "a\\b"));
        // '?' matches exactly one non-separator char.
        assert!(super::perm_glob_match("a?c", "abc"));
        assert!(!super::perm_glob_match("a?c", "a/c"));
    }

    #[test]
    fn append_allow_rule_escapes_glob_chars_in_path() {
        let ws = tempfile::tempdir().unwrap();
        super::append_allow_rule(ws.path(), "write_file", &json!({"path": "src/*.rs"})).unwrap();
        let rules = super::load_permission_rules(ws.path());
        assert_eq!(rules.len(), 1);
        assert_eq!(
            super::evaluate_permission_rules(&rules, "write_file", &json!({"path": "src/*.rs"})),
            Some(super::PermissionDecision::Allow),
            "the escaped glob must still match the literal approved path"
        );
        assert_eq!(
            super::evaluate_permission_rules(&rules, "write_file", &json!({"path": "src/main.rs"})),
            None,
            "an escaped path glob must not widen into a wildcard"
        );
    }

    #[test]
    fn allow_always_skip_reason_refuses_glob_in_command() {
        assert!(
            super::allow_always_skip_reason(&json!({"command": "rm -rf /tmp/*"})).is_some(),
            "a command containing a glob char must not persist as a rule pattern"
        );
        assert!(super::allow_always_skip_reason(&json!({"command": "git status"})).is_none());
    }

    #[test]
    fn modified_input_denied_blocks_rule_denied_rewrite() {
        let ws = tempfile::tempdir().unwrap();
        let dir = ws.path().join(".smedja");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("workspace.toml"),
            "[[permission.rules]]\ntool = \"bash\"\ncommand_pattern = \"rm *\"\nmode = \"deny\"\n",
        )
        .unwrap();
        assert!(
            super::modified_input_denied(ws.path(), "bash", "{\"command\": \"rm -rf /tmp/x\"}")
                .is_some(),
            "a modified call matching a deny rule must be re-blocked"
        );
        assert!(
            super::modified_input_denied(ws.path(), "bash", "{\"command\": \"ls\"}").is_none(),
            "a modified call matching no deny rule proceeds"
        );
        assert!(
            super::modified_input_denied(ws.path(), "bash", "not json").is_none(),
            "a non-JSON instruction is rejected upstream, not here"
        );
    }

    // ── scrubbing ────────────────────────────────────────────────────────────

    #[test]
    fn scrub_redacts_secret_keyed_values() {
        let args = json!({
            "command": "deploy",
            "api_token": "abc123",
            "Authorization": "Bearer xyz",
            "nested": { "db_password": "hunter2", "note": "ok" },
        });
        let scrubbed = super::scrub_args(&args);
        assert_eq!(scrubbed["command"], "deploy");
        assert_eq!(scrubbed["api_token"], "[redacted]");
        assert_eq!(scrubbed["Authorization"], "[redacted]");
        assert_eq!(scrubbed["nested"]["db_password"], "[redacted]");
        assert_eq!(scrubbed["nested"]["note"], "ok");
    }

    #[test]
    fn scrub_redacts_bearer_looking_strings_but_keeps_paths_and_commands() {
        let args = json!({
            "path": "/home/user/project/src/main.rs",
            "command": "git status --short",
            "token_value": "ghp_a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p6",
            // Neutral key, but the value is a long high-entropy bearer-looking string.
            "note": "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.Kx9fQ2mZv8rLw4nT7bYc3s",
            "run_id": "123e4567-e89b-42d3-a456-426614174000",
        });
        let scrubbed = super::scrub_args(&args);
        assert_eq!(scrubbed["path"], "/home/user/project/src/main.rs");
        assert_eq!(scrubbed["command"], "git status --short");
        assert_eq!(scrubbed["token_value"], "[redacted]");
        assert_eq!(scrubbed["note"], "[redacted]");
        // UUIDs are low-entropy identifiers, not secrets — keep them readable.
        assert_eq!(scrubbed["run_id"], "123e4567-e89b-42d3-a456-426614174000");
        // Short, low-entropy strings survive even without a suspicious key.
        assert_eq!(super::scrub_args(&json!("ls -la")), json!("ls -la"));
    }

    // ── rules + allow-always on the shared gate path ─────────────────────────

    #[tokio::test]
    async fn gate_tool_honors_persisted_allow_rule_without_suspending() {
        let ws = tempfile::tempdir().unwrap();
        super::append_allow_rule(ws.path(), "bash", &json!({"command": "git status"})).unwrap();
        let gate = CoworkGate::default(); // Ask mode — would suspend without the rule.
        let ctx = super::GateContext {
            agent: None,
            workspace: Some(ws.path().to_path_buf()),
            supports_modify: true,
        };
        let decision = gate
            .gate_tool(1, "bash", json!({"command": "git status"}), "", &ctx, None)
            .await;
        assert!(matches!(decision.decision, Decision::Approve));
        assert!(gate.list_pending().await.is_empty());
    }

    #[tokio::test]
    async fn gate_tool_honors_persisted_deny_rule() {
        let ws = tempfile::tempdir().unwrap();
        let dir = ws.path().join(".smedja");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("workspace.toml"),
            "[[permission.rules]]\ntool = \"bash\"\ncommand_pattern = \"rm *\"\nmode = \"deny\"\n",
        )
        .unwrap();
        let gate = CoworkGate::default();
        let ctx = super::GateContext {
            agent: None,
            workspace: Some(ws.path().to_path_buf()),
            supports_modify: true,
        };
        let decision = gate
            .gate_tool(
                1,
                "bash",
                json!({"command": "rm -rf /tmp/x"}),
                "",
                &ctx,
                None,
            )
            .await;
        assert!(
            matches!(decision.decision, Decision::Deny(ref r) if r.contains("permission rule"))
        );
        assert!(gate.list_pending().await.is_empty());
    }

    #[tokio::test]
    async fn approve_always_via_gate_tool_persists_rule() {
        let ws = tempfile::tempdir().unwrap();
        let gate = Arc::new(CoworkGate::default());
        let ctx = super::GateContext {
            agent: Some("claude".into()),
            workspace: Some(ws.path().to_path_buf()),
            supports_modify: true,
        };
        let g2 = Arc::clone(&gate);
        let handle = tokio::spawn(async move {
            g2.gate_tool(0, "bash", json!({"command": "make test"}), "", &ctx, None)
                .await
        });
        let id = {
            let mut found = None;
            for _ in 0..1000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("pending approval should appear")
        };
        assert!(gate.approve_always(&id).await.0);
        assert!(matches!(handle.await.unwrap().decision, Decision::Approve));
        let rules = super::load_permission_rules(ws.path());
        assert_eq!(rules.len(), 1, "allow-always must persist a rule");
        assert_eq!(rules[0].tool, "bash");
        assert_eq!(rules[0].mode, super::RuleMode::Allow);
        assert_eq!(
            rules[0].command_pattern.as_deref(),
            Some("make test"),
            "rule must be scoped to the approved command"
        );
    }

    // ── modify contract ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn modify_with_non_object_instruction_is_rejected_and_stays_pending() {
        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);
        let handle = tokio::spawn(async move { gate2.intercept(prompt(), 0, None).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let id = gate.list_pending().await[0].0.clone();

        for bad in ["use a safer path", "42", "[1,2]"] {
            let err = gate.modify(&id, bad.into()).await.unwrap_err();
            assert!(
                matches!(err, super::ModifyRejection::InvalidInstruction(_)),
                "{bad:?} must be rejected as invalid, got {err:?}"
            );
            assert_eq!(
                gate.list_pending().await.len(),
                1,
                "a rejected modify must leave the prompt pending"
            );
        }
        // A valid JSON object resolves with Modify carrying the replacement.
        gate.modify(&id, r#"{"cmd":"ls -a"}"#.into()).await.unwrap();
        assert!(matches!(
            handle.await.unwrap(),
            Decision::Modify(ref i) if i == r#"{"cmd":"ls -a"}"#
        ));
    }

    #[tokio::test]
    async fn modify_on_unsupported_backend_is_rejected() {
        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);
        let mut p = prompt();
        p.supports_modify = false;
        let handle = tokio::spawn(async move { gate2.intercept(p, 0, None).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let id = gate.list_pending().await[0].0.clone();

        let err = gate
            .modify(&id, r#"{"cmd":"ls"}"#.into())
            .await
            .unwrap_err();
        assert!(matches!(err, super::ModifyRejection::Unsupported { .. }));
        assert_eq!(gate.list_pending().await.len(), 1);
        gate.approve(&id).await;
        handle.await.unwrap();
    }

    // ── timeout notice ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn timeout_publishes_notice_on_the_stream() {
        use smedja_bellows::Dispatcher;

        let gate = CoworkGate::default();
        let dispatcher = Arc::new(Dispatcher::new(16));
        let mut rx = dispatcher.subscribe();

        let decision = gate
            .intercept(prompt(), 1, Some((dispatcher.as_ref(), Some("t-to"))))
            .await;
        assert!(matches!(decision, Decision::Deny(ref r) if r == "timeout"));

        let mut saw_notice = false;
        while let Ok(ev) = rx.try_recv() {
            if let smedja_bellows::TurnEvent::AssistantDelta { ref content, .. } = ev {
                if content.contains("timed out") {
                    saw_notice = true;
                }
            }
        }
        assert!(saw_notice, "a gate timeout must publish a visible notice");
    }

    #[tokio::test]
    async fn timeout_broadcasts_cowork_resolved() {
        use smedja_bellows::{CoworkOutcome, Dispatcher};

        // The AssistantDelta notice is dropped on turn_id-less paths (hook/ACP);
        // the cowork_resolved event is the resolution signal there.
        let gate = CoworkGate::default();
        let dispatcher = Arc::new(Dispatcher::new(16));
        let mut rx = dispatcher.subscribe();

        let decision = gate
            .intercept(prompt(), 1, Some((dispatcher.as_ref(), None)))
            .await;
        assert!(matches!(decision, Decision::Deny(ref r) if r == "timeout"));

        let mut saw_resolved = false;
        while let Ok(ev) = rx.try_recv() {
            if let smedja_bellows::TurnEvent::CoworkResolved {
                ref approval_id,
                outcome,
            } = ev
            {
                assert_eq!(outcome, CoworkOutcome::Timeout);
                assert!(!approval_id.is_empty());
                saw_resolved = true;
            }
        }
        assert!(
            saw_resolved,
            "a gate timeout must broadcast cowork_resolved"
        );
    }

    #[tokio::test]
    async fn resolve_broadcasts_cowork_resolved() {
        use smedja_bellows::{CoworkOutcome, Dispatcher};

        let gate = Arc::new(CoworkGate::default());
        let gate2 = Arc::clone(&gate);
        let dispatcher = Arc::new(Dispatcher::new(16));
        let mut rx = dispatcher.subscribe();
        let d2 = Arc::clone(&dispatcher);

        let handle = tokio::spawn(async move {
            gate2
                .intercept(prompt(), 0, Some((d2.as_ref(), Some("t-r"))))
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let id = gate.list_pending().await[0].0.clone();

        assert!(gate.approve(&id).await);
        assert!(matches!(handle.await.unwrap(), Decision::Approve));

        let mut saw_request = false;
        let mut saw_resolved = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                smedja_bellows::TurnEvent::CoworkRequest { .. } => saw_request = true,
                smedja_bellows::TurnEvent::CoworkResolved {
                    ref approval_id,
                    outcome,
                } => {
                    assert_eq!(approval_id, &id);
                    assert_eq!(outcome, CoworkOutcome::Approved);
                    saw_resolved = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_request && saw_resolved,
            "request AND resolution must both be broadcast"
        );
    }

    // ── command_pattern exact-match (allow-always prefix broadening) ─────────

    #[test]
    fn command_pattern_without_star_is_exact_match() {
        let rules = vec![super::PermissionRule {
            tool: "bash".into(),
            path_glob: None,
            command_pattern: Some("git status".into()),
            mode: super::RuleMode::Allow,
        }];
        // The exact approved command is allowed…
        assert_eq!(
            super::evaluate_permission_rules(&rules, "bash", &json!({"command": "git status"})),
            Some(super::PermissionDecision::Allow)
        );
        // …but anything beyond it — especially a chained payload — is not.
        for evil in [
            "git status && true",
            "git status && rm -rf ~",
            "git status-extra",
            "git status ",
        ] {
            assert_eq!(
                super::evaluate_permission_rules(&rules, "bash", &json!({"command": evil})),
                None,
                "{evil:?} must NOT match the exact rule"
            );
        }
        // A trailing `*` still opts into prefix semantics.
        let prefix_rules = vec![super::PermissionRule {
            tool: "bash".into(),
            path_glob: None,
            command_pattern: Some("git status*".into()),
            mode: super::RuleMode::Allow,
        }];
        assert_eq!(
            super::evaluate_permission_rules(
                &prefix_rules,
                "bash",
                &json!({"command": "git status --short"})
            ),
            Some(super::PermissionDecision::Allow),
            "a trailing * must keep prefix semantics"
        );
    }

    #[tokio::test]
    async fn allow_always_on_git_status_does_not_allow_chained_command() {
        // End-to-end regression: approving `git status` with scope "always"
        // persists a rule that must NOT allow `git status && true`.
        let ws = tempfile::tempdir().unwrap();
        super::append_allow_rule(ws.path(), "bash", &json!({"command": "git status"})).unwrap();
        let gate = Arc::new(CoworkGate::default()); // Ask mode.
        let ctx = super::GateContext {
            agent: None,
            workspace: Some(ws.path().to_path_buf()),
            supports_modify: true,
        };

        // The exact command bypasses the gate…
        let out = gate
            .gate_tool(1, "bash", json!({"command": "git status"}), "", &ctx, None)
            .await;
        assert!(matches!(out.decision, Decision::Approve));
        assert!(gate.list_pending().await.is_empty());

        // …but the chained command still suspends for a human decision.
        let g2 = Arc::clone(&gate);
        let handle = tokio::spawn(async move {
            g2.gate_tool(
                1,
                "bash",
                json!({"command": "git status && true"}),
                "",
                &ctx,
                None,
            )
            .await
        });
        let id = {
            let mut found = None;
            for _ in 0..1000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("chained command must suspend — it must not match the exact rule")
        };
        gate.deny(&id, "not the approved command".into()).await;
        assert!(matches!(handle.await.unwrap().decision, Decision::Deny(_)));
    }

    // ── allow-always persistence vetoes (secret payload / tool-only) ─────────

    #[test]
    fn allow_always_skip_reason_scopes_and_vetoes() {
        // Scoped, clean payloads persist.
        assert_eq!(
            super::allow_always_skip_reason(&json!({"command": "git status"})),
            None
        );
        assert_eq!(
            super::allow_always_skip_reason(&json!({"path": "src/main.rs"})),
            None
        );
        // Tool-only args (no command/path) decline to persist.
        assert_eq!(
            super::allow_always_skip_reason(&json!({"content": "hello"})),
            Some("always not available for this tool (no command/path to scope the rule)")
        );
        assert_eq!(
            super::allow_always_skip_reason(&serde_json::Value::Null),
            Some("always not available for this tool (no command/path to scope the rule)")
        );
        // A secret-looking value anywhere in the payload vetoes persistence.
        assert_eq!(
            super::allow_always_skip_reason(
                &json!({"command": "curl -H \"Authorization: Bearer sk-abc123xyz\" https://x"})
            ),
            Some("rule not persisted: command contains a secret-looking value")
        );
        assert_eq!(
            super::allow_always_skip_reason(&json!({"command": "ghp_a1b2c3d4e5f6g7h8i9j0"})),
            Some("rule not persisted: command contains a secret-looking value")
        );
    }

    #[tokio::test]
    async fn approve_always_with_secret_in_command_approves_once_but_persists_nothing() {
        let ws = tempfile::tempdir().unwrap();
        let gate = Arc::new(CoworkGate::default());
        let ctx = super::GateContext {
            agent: None,
            workspace: Some(ws.path().to_path_buf()),
            supports_modify: true,
        };
        let g2 = Arc::clone(&gate);
        let handle = tokio::spawn(async move {
            g2.gate_tool(
                0,
                "bash",
                json!({"command": "curl -H \"Authorization: Bearer sk-live-secret-token-123\" https://api.example.com"}),
                "",
                &ctx,
                None,
            )
            .await
        });
        let id = {
            let mut found = None;
            for _ in 0..1000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("pending approval should appear")
        };
        assert!(gate.approve_always(&id).await.0);
        let outcome = handle.await.unwrap();
        // The single approval proceeds…
        assert!(matches!(outcome.decision, Decision::Approve));
        // …but nothing is written to workspace.toml, and the note says why.
        assert_eq!(
            outcome.note.as_deref(),
            Some("rule not persisted: command contains a secret-looking value")
        );
        assert!(
            super::load_permission_rules(ws.path()).is_empty(),
            "a secret-bearing payload must never persist a rule"
        );
    }

    #[tokio::test]
    async fn approve_always_tool_only_approves_once_but_persists_nothing() {
        let ws = tempfile::tempdir().unwrap();
        let gate = Arc::new(CoworkGate::default());
        let ctx = super::GateContext {
            agent: None,
            workspace: Some(ws.path().to_path_buf()),
            supports_modify: true,
        };
        let g2 = Arc::clone(&gate);
        // `apply_patch`-style call whose args carry no path/command to scope to.
        let handle = tokio::spawn(async move {
            g2.gate_tool(0, "apply_patch", json!({"patch": "@@ ..."}), "", &ctx, None)
                .await
        });
        let id = {
            let mut found = None;
            for _ in 0..1000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("pending approval should appear")
        };
        assert!(gate.approve_always(&id).await.0);
        let outcome = handle.await.unwrap();
        assert!(matches!(outcome.decision, Decision::Approve));
        assert_eq!(
            outcome.note.as_deref(),
            Some("always not available for this tool (no command/path to scope the rule)")
        );
        assert!(
            super::load_permission_rules(ws.path()).is_empty(),
            "a tool-only allow-always must not persist a blanket rule"
        );
    }

    // ── Ask rules, cancellation, timeout race ────────────────────────────────

    #[tokio::test]
    async fn ask_rule_forces_suspend_even_in_auto_mode() {
        let ws = tempfile::tempdir().unwrap();
        let dir = ws.path().join(".smedja");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("workspace.toml"),
            "[[permission.rules]]\ntool = \"bash\"\nmode = \"ask\"\n",
        )
        .unwrap();
        let gate = Arc::new(CoworkGate::default());
        gate.set_mode(PermissionMode::Auto).await;
        let ctx = super::GateContext {
            agent: None,
            workspace: Some(ws.path().to_path_buf()),
            supports_modify: true,
        };
        let g2 = Arc::clone(&gate);
        let handle = tokio::spawn(async move {
            g2.gate_tool(0, "bash", json!({"command": "ls"}), "", &ctx, None)
                .await
        });
        let id = {
            let mut found = None;
            for _ in 0..1000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("an Ask rule must suspend even under Auto mode")
        };
        assert!(gate.approve(&id).await);
        assert!(matches!(handle.await.unwrap().decision, Decision::Approve));
    }

    #[tokio::test]
    async fn forced_ask_honors_persisted_allow_and_deny_rules() {
        let ws = tempfile::tempdir().unwrap();
        let dir = ws.path().join(".smedja");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("workspace.toml"),
            "[[permission.rules]]\ntool = \"bash\"\ncommand_pattern = \"ls\"\nmode = \"allow\"\n\n[[permission.rules]]\ntool = \"terraform\"\nmode = \"deny\"\n",
        )
        .unwrap();
        let gate = CoworkGate::default();
        let ctx = super::GateContext {
            agent: None,
            workspace: Some(ws.path().to_path_buf()),
            supports_modify: true,
        };
        let allow = gate
            .gate_tool_forced_ask(0, "bash", json!({"command": "ls"}), "", &ctx, None)
            .await;
        assert!(
            matches!(allow.decision, Decision::Approve),
            "a persisted allow rule must short-circuit forced_ask"
        );
        let deny = gate
            .gate_tool_forced_ask(0, "terraform", json!({}), "", &ctx, None)
            .await;
        assert!(
            matches!(deny.decision, Decision::Deny(ref r) if r.contains("permission rule")),
            "a persisted deny rule must deny without suspending"
        );
        assert!(gate.list_pending().await.is_empty());
    }

    #[tokio::test]
    async fn approve_always_response_carries_persistence_veto_note() {
        let gate = Arc::new(CoworkGate::default());
        let g2 = Arc::clone(&gate);
        // Tool-only prompt: no command/path to scope a rule to.
        let handle = tokio::spawn(async move {
            g2.intercept_tracked(
                ApprovalPrompt::new(1, "apply_patch", &json!({"patch": "@@ ..."}), ""),
                0,
                None,
            )
            .await
        });
        let id = {
            let mut found = None;
            for _ in 0..1000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("pending approval should appear")
        };
        let (found, note) = gate.approve_always(&id).await;
        assert!(found);
        assert_eq!(
            note.as_deref(),
            Some("always not available for this tool (no command/path to scope the rule)"),
            "the RPC responder must learn the allow-always cannot persist"
        );
        assert!(matches!(handle.await.unwrap().1, Decision::Approve));
    }

    #[tokio::test]
    async fn cancelled_suspension_broadcasts_cancelled_outcome() {
        use smedja_bellows::{CoworkOutcome, Dispatcher};

        let gate = Arc::new(CoworkGate::default());
        let g2 = Arc::clone(&gate);
        let dispatcher = Arc::new(Dispatcher::new(16));
        let mut rx = dispatcher.subscribe();
        let d2 = Arc::clone(&dispatcher);
        let handle = tokio::spawn(async move {
            g2.intercept_tracked(prompt(), 0, Some((d2.as_ref(), None)))
                .await
        });
        let id = {
            let mut found = None;
            for _ in 0..1000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("pending approval should appear")
        };
        // The waiter goes away without a decision (TUI disconnect / walk-away).
        handle.abort();
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match rx.recv().await {
                    Ok(smedja_bellows::TurnEvent::CoworkResolved {
                        approval_id,
                        outcome,
                    }) if approval_id == id => {
                        break outcome;
                    }
                    Ok(_) => continue,
                    Err(e) => panic!("dispatcher closed before cancelled outcome: {e}"),
                }
            }
        })
        .await
        .expect("a cancelled suspension must broadcast cowork_resolved");
        assert_eq!(ev, CoworkOutcome::Cancelled);
        assert!(gate.list_pending().await.is_empty());
    }

    #[tokio::test]
    async fn timed_out_approval_cannot_be_resolved_late() {
        use smedja_bellows::Dispatcher;

        let gate = Arc::new(CoworkGate::default());
        let g2 = Arc::clone(&gate);
        let dispatcher = Arc::new(Dispatcher::new(16));
        let d2 = Arc::clone(&dispatcher);
        let handle = tokio::spawn(async move {
            g2.intercept_tracked(prompt(), 1, Some((d2.as_ref(), None)))
                .await
        });
        let id = {
            let mut found = None;
            for _ in 0..1000 {
                if let Some((id, _)) = gate.list_pending().await.first() {
                    found = Some(id.clone());
                    break;
                }
                tokio::task::yield_now().await;
            }
            found.expect("pending approval should appear")
        };
        let (returned_id, decision) = handle.await.unwrap();
        assert_eq!(returned_id, id);
        assert!(matches!(decision, Decision::Deny(ref r) if r == "timeout"));
        assert!(
            !gate.approve(&id).await,
            "a timed-out approval must be gone before the broadcast, so a late resolve finds nothing"
        );
        assert!(gate.list_pending().await.is_empty());
    }

    // ── scrubbing: hex + known-prefix secrets ────────────────────────────────

    #[test]
    fn scrub_redacts_hex_tokens_and_known_prefixes() {
        let args = json!({
            // 32+ pure hex — entropy ~4.0 bits/byte, below the old threshold.
            "digest": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90ff",
            "upper_hex": "A1B2C3D4E5F60718293A4B5C6D7E8F90A1B2C3D4E5F60718293A4B5C6D7E8F90FF",
            // Known credential prefixes, some below the entropy floor.
            "api": "sk-proj-short",
            "old_github": "ghp_a1b2c3d4e5f6",
            "oauth": "gho_16C7e42F292c6912E7710c838347Ae178B4a",
            "slack_bot": "xoxb-123-456-abc",
            "slack_user": "xoxp-123-456",
            "aws": "AKIAIOSFODNN7EXAMPLE",
            "jwt": "eyJhbGciOiJIUzI1NiJ9.x.y",
            // Non-secrets survive.
            "command": "git status --short",
            "run_id": "123e4567-e89b-42d3-a456-426614174000",
        });
        let scrubbed = super::scrub_args(&args);
        for key in [
            "digest",
            "upper_hex",
            "api",
            "old_github",
            "oauth",
            "slack_bot",
            "slack_user",
            "aws",
            "jwt",
        ] {
            assert_eq!(scrubbed[key], "[redacted]", "{key} must be redacted");
        }
        assert_eq!(scrubbed["command"], "git status --short");
        assert_eq!(scrubbed["run_id"], "123e4567-e89b-42d3-a456-426614174000");
    }

    // ── bounded display args ─────────────────────────────────────────────────

    #[test]
    fn prompt_caps_megabyte_args_display() {
        let huge = "x".repeat(1024 * 1024);
        let p = ApprovalPrompt::new(1, "bash", &json!({"command": huge}), "");
        let s = p.args_scrubbed.to_string();
        assert!(
            s.len() <= ApprovalPrompt::MAX_ARGS_DISPLAY + 32,
            "display args must be capped; got {} bytes",
            s.len()
        );
        assert!(
            s.contains("…[truncated]"),
            "a capped payload must carry the truncation marker"
        );
    }

    #[test]
    fn prompt_keeps_small_args_untouched() {
        let p = ApprovalPrompt::new(1, "bash", &json!({"command": "ls"}), "");
        assert_eq!(p.args_scrubbed, json!({"command": "ls"}));
    }

    #[test]
    fn prompt_caps_multibyte_args_without_splitting_a_char() {
        // The cap is in CHARS, not bytes: multibyte content must never be cut
        // mid-sequence (a byte-wise cut would panic or corrupt the string).
        let huge = "å".repeat(ApprovalPrompt::MAX_ARGS_DISPLAY * 2);
        let p = ApprovalPrompt::new(1, "bash", &json!({"command": huge}), "");
        let s = p.args_scrubbed.to_string();
        assert!(s.contains("…[truncated]"), "cap marker: {s:.80}");
        // The serialised object is cut at MAX_ARGS_DISPLAY chars: the
        // `{"command":"` prefix (12 chars) precedes the payload in the cut.
        assert_eq!(
            s.matches('å').count(),
            ApprovalPrompt::MAX_ARGS_DISPLAY - 12,
            "every retained char must be intact"
        );
    }

    // ── scrubbing: arrays and bearer boundaries ─────────────────────────────

    #[test]
    fn scrub_redacts_bearer_looking_strings_inside_arrays() {
        // Secrets nested in arrays (e.g. an argv list) are redacted too —
        // only object values were exercised above.
        let args = json!({
            "argv": ["git", "clone", "https://xoxb-123-456-abc@example.com/repo"],
            "env": ["PATH=/usr/bin", "ghp_a1b2c3d4e5f6g7h8i9j0k1l2m3n4o5p6"],
        });
        let scrubbed = super::scrub_args(&args);
        assert_eq!(scrubbed["argv"][0], "git");
        // The URL merely CONTAINS a bearer-looking substring; scrubbing is
        // whole-string only (and a '/' vetoes the heuristic), so it survives —
        // the allow-always veto is the layer that refuses to persist such
        // payloads.
        assert_eq!(
            scrubbed["argv"][2],
            "https://xoxb-123-456-abc@example.com/repo"
        );
        assert_eq!(scrubbed["env"][0], "PATH=/usr/bin");
        // A bare token as an array element IS redacted.
        assert_eq!(scrubbed["env"][1], "[redacted]");
        // So is a long whitespace-free high-entropy word with no known prefix.
        let bare = super::scrub_args(&json!(["k9QmzX2vB7nL4pW8sD1fG6hJk3Rt5yUi"]));
        assert_eq!(bare[0], "[redacted]");
    }

    #[test]
    fn looks_like_bearer_length_boundary() {
        // Below MIN_LEN (24) a prefix-less high-entropy string is not a bearer…
        let short = "k9QmzX2vB7nL4pW8sD1fG6h"; // 23 chars, all distinct
        assert_eq!(short.len(), 23);
        assert!(
            !super::looks_like_bearer(short),
            "23-char string must stay readable"
        );
        // …at 24 distinct chars the entropy rule fires (log2(24) ≈ 4.58 ≥ 4.2).
        let long = "k9QmzX2vB7nL4pW8sD1fG6hJ";
        assert_eq!(long.len(), 24);
        assert!(
            super::looks_like_bearer(long),
            "24-char high-entropy string must read as a bearer token"
        );
        // Whitespace or a path separator vetoes the heuristic at any length.
        assert!(!super::looks_like_bearer(&format!("{long} {long}")));
        assert!(!super::looks_like_bearer(&format!("/{long}/{long}")));
    }

    // ── derived popup rationale ──────────────────────────────────────────────

    #[test]
    fn derive_reasoning_prefers_command_then_path_then_json() {
        assert_eq!(
            super::derive_reasoning("bash", &json!({"command": "ls -la"})),
            "run: ls -la"
        );
        // `cmd` is the alternate spelling.
        assert_eq!(
            super::derive_reasoning("bash", &json!({"cmd": "pwd"})),
            "run: pwd"
        );
        assert_eq!(
            super::derive_reasoning("write_file", &json!({"path": "src/main.rs"})),
            "write_file: src/main.rs"
        );
        // No command/path: a compact JSON rendering.
        let derived = super::derive_reasoning("mystery_tool", &json!({"x": 1}));
        assert!(derived.contains("\"x\":1"), "got: {derived}");
        // Null args derive nothing.
        assert_eq!(
            super::derive_reasoning("bash", &serde_json::Value::Null),
            ""
        );
    }

    #[test]
    fn derive_reasoning_truncates_multibyte_safely() {
        // Over the 160-char budget the rationale is cut with an ellipsis —
        // char-based, so multibyte content must not panic or split a char.
        let long = format!("run: {}", "å".repeat(500));
        let derived = super::derive_reasoning("bash", &json!({"command": long}));
        assert!(derived.ends_with('…'), "ellipsis marker: {derived:.40}");
        assert_eq!(derived.chars().count(), 161, "160 chars + ellipsis");
    }

    // ── risk labels ──────────────────────────────────────────────────────────

    #[test]
    fn tool_risk_labels_match_tool_kind() {
        assert_eq!(super::tool_risk("read_file"), "read_only");
        assert_eq!(super::tool_risk("write_file"), "edit");
        assert_eq!(super::tool_risk("bash"), "exec");
        // Unknown tools are conservatively exec.
        assert_eq!(super::tool_risk("mystery_tool"), "exec");
    }
}
