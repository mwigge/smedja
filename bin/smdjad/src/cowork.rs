//! Human-in-the-loop gate for tool calls in cowork mode.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use smedja_bellows::{CorrelationCtx, Dispatcher, TurnEvent};
use tokio::sync::{oneshot, Mutex};

/// Describes a pending tool call awaiting human approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalPrompt {
    pub step_n: u32,
    pub tool: String,
    /// Args with any secret values scrubbed.
    pub args_scrubbed: serde_json::Value,
    pub reasoning: String,
    pub plan_summary: String,
}

/// The human's decision on a pending tool call.
#[derive(Debug, Clone)]
pub enum Decision {
    Approve,
    Deny(String),
    Modify(String),
}

/// Unique ID for a pending approval request.
pub type ApprovalId = String;

/// A pending approval awaiting a human decision.
struct PendingApproval {
    prompt: ApprovalPrompt,
    /// Sender half of the oneshot; the receiver suspends in [`CoworkGate::intercept`].
    tx: oneshot::Sender<Decision>,
}

/// RAII guard that removes a pending entry from the map on **every** exit path
/// of [`CoworkGate::intercept`] — normal resolution, timeout, sender-dropped, or
/// future cancellation (TUI disconnect / walk-away). Without this, the timeout
/// and sender-dropped branches returned `Deny` while leaving the map entry
/// behind, leaking one entry per unanswered prompt forever.
///
/// [`CoworkGate::resolve`] may already have removed the entry on the happy path;
/// the removal here is then a harmless no-op.
struct PendingGuard {
    pending: Arc<Mutex<HashMap<ApprovalId, PendingApproval>>>,
    id: ApprovalId,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        // Fast path: uncontended lock, remove synchronously. If the map is
        // momentarily locked (e.g. a concurrent resolve), offload the removal to
        // the runtime so Drop never blocks. `intercept` always runs on a Tokio
        // runtime, so `tokio::spawn` is available.
        if let Ok(mut map) = self.pending.try_lock() {
            map.remove(&self.id);
        } else {
            let pending = Arc::clone(&self.pending);
            let id = std::mem::take(&mut self.id);
            tokio::spawn(async move {
                pending.lock().await.remove(&id);
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

/// A single declarative permission rule from `[[permission.rules]]` in
/// `.smedja/workspace.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct PermissionRule {
    /// Tool name or glob pattern (e.g. `"bash"`, `"write_*"`).
    pub tool: String,
    /// Glob matched against the `path` field of file-tool inputs.
    pub path_glob: Option<String>,
    /// Prefix/glob matched against the `command` field of bash inputs.
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
#[must_use]
pub fn load_permission_rules(workspace: &std::path::Path) -> Vec<PermissionRule> {
    #[derive(Deserialize, Default)]
    struct WorkspaceToml {
        permission: Option<PermSection>,
    }
    #[derive(Deserialize, Default)]
    struct PermSection {
        rules: Option<Vec<PermissionRule>>,
    }
    let path = workspace.join(".smedja").join("workspace.toml");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| toml::from_str::<WorkspaceToml>(&s).ok())
        .and_then(|c| c.permission?.rules)
        .unwrap_or_default()
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
/// so an appended block is picked up alongside the pre-existing rules.
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
    #[derive(Serialize)]
    struct Wrapper {
        permission: PermWrap,
    }
    #[derive(Serialize)]
    struct PermWrap {
        rules: Vec<PersistRule>,
    }
    #[derive(Serialize)]
    struct PersistRule {
        tool: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path_glob: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        command_pattern: Option<String>,
        mode: String,
    }

    // Scope the rule to the concrete target when the args expose one, so an
    // allow-always is as narrow as the call the user actually approved.
    let path_glob = args
        .get("path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let command_pattern = args
        .get("command")
        .or_else(|| args.get("cmd"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let block = toml::to_string(&Wrapper {
        permission: PermWrap {
            rules: vec![PersistRule {
                tool: tool.to_owned(),
                path_glob,
                command_pattern,
                mode: "allow".to_owned(),
            }],
        },
    })
    .map_err(std::io::Error::other)?;

    let dir = workspace.join(".smedja");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("workspace.toml");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&block);
    std::fs::write(&path, out)
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
            let prefix = pat.trim_end_matches('*');
            if !cmd.starts_with(prefix) {
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

/// Minimal glob: `*` matches any sequence of characters, `?` matches one char.
fn perm_glob_match(pattern: &str, value: &str) -> bool {
    let mut p = pattern.as_bytes();
    let mut s = value.as_bytes();
    loop {
        match (p.first(), s.first()) {
            (None, None) => return true,
            (Some(&b'*'), _) => {
                p = &p[1..];
                if p.is_empty() {
                    return true;
                }
                for i in 0..=s.len() {
                    if perm_glob_match(
                        std::str::from_utf8(p).unwrap_or(""),
                        std::str::from_utf8(&s[i..]).unwrap_or(""),
                    ) {
                        return true;
                    }
                }
                return false;
            }
            (Some(&b'?'), Some(_)) => {
                p = &p[1..];
                s = &s[1..];
            }
            (Some(a), Some(b)) if a == b => {
                p = &p[1..];
                s = &s[1..];
            }
            _ => return false,
        }
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
                turn_id: turn_id.map(str::to_owned),
                correlation: CorrelationCtx::default(),
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
    /// Returns `true` if the approval ID was found and resolved.
    pub async fn approve_always(&self, id: &str) -> bool {
        self.always_ids.lock().await.insert(id.to_owned());
        let found = self.resolve(id, Decision::Approve).await;
        if !found {
            // The id was unknown (already resolved / timed out): don't leak a
            // dangling always-flag.
            self.always_ids.lock().await.remove(id);
        }
        found
    }

    /// Consumes and returns whether `id` was resolved with an allow-always scope
    /// (see [`Self::approve_always`]). Idempotent: a second call returns `false`.
    pub async fn take_always(&self, id: &str) -> bool {
        self.always_ids.lock().await.remove(id)
    }

    /// Resolves a pending approval with [`Decision::Deny`].
    ///
    /// Returns `true` if the approval ID was found and resolved.
    pub async fn deny(&self, id: &str, reason: String) -> bool {
        self.resolve(id, Decision::Deny(reason)).await
    }

    /// Resolves a pending approval with [`Decision::Modify`].
    ///
    /// Returns `true` if the approval ID was found and resolved.
    pub async fn modify(&self, id: &str, instruction: String) -> bool {
        self.resolve(id, Decision::Modify(instruction)).await
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
    /// allow/deny outright per [`evaluate`], or — for `Ask` — suspend on the
    /// gate (≤30 min) until the user decides. Returns the resolved [`Decision`].
    ///
    /// Pass `push` to have a [`TurnEvent::CoworkRequest`] pushed via the NDJSON
    /// stream so the TUI receives it without polling.
    pub async fn gate_tool(
        &self,
        step_n: u32,
        tool: &str,
        args_scrubbed: serde_json::Value,
        reasoning: &str,
        push: Option<(&Dispatcher, Option<&str>)>,
    ) -> Decision {
        let mode = self.mode().await;
        match evaluate(mode, tool) {
            PermissionDecision::Allow => Decision::Approve,
            PermissionDecision::Deny => {
                Decision::Deny(format!("blocked by {} mode", mode.as_str()))
            }
            PermissionDecision::Ask => {
                self.intercept(
                    ApprovalPrompt {
                        step_n,
                        tool: tool.to_owned(),
                        args_scrubbed,
                        reasoning: reasoning.to_owned(),
                        plan_summary: String::new(),
                    },
                    30 * 60,
                    push,
                )
                .await
            }
        }
    }

    /// Like [`Self::gate_tool`] but always suspends for a human decision,
    /// ignoring the mode's allow/auto — for high-risk roles (`IaC`) whose
    /// mutations must be confirmed even under `AcceptEdits`/`Auto`.
    pub async fn gate_tool_forced_ask(
        &self,
        step_n: u32,
        tool: &str,
        args_scrubbed: serde_json::Value,
        reasoning: &str,
        push: Option<(&Dispatcher, Option<&str>)>,
    ) -> Decision {
        self.intercept(
            ApprovalPrompt {
                step_n,
                tool: tool.to_owned(),
                args_scrubbed,
                reasoning: reasoning.to_owned(),
                plan_summary: String::new(),
            },
            30 * 60,
            push,
        )
        .await
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
        ApprovalPrompt {
            step_n: 1,
            tool: "bash".into(),
            args_scrubbed: json!({"cmd": "ls"}),
            reasoning: "list files".into(),
            plan_summary: "exploration".into(),
        }
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
                                          // Read-only: allowed, no pending entry.
        assert!(matches!(
            gate.gate_tool(1, "read_file", json!({}), "", None).await,
            Decision::Approve
        ));
        assert!(gate.list_pending().await.is_empty());

        // Plan mode denies a write outright.
        gate.set_mode(PermissionMode::Plan).await;
        assert!(matches!(
            gate.gate_tool(1, "write_file", json!({}), "", None).await,
            Decision::Deny(_)
        ));

        // Ask mode suspends; approving concurrently resolves it.
        let gate = Arc::new(CoworkGate::default());
        let g2 = Arc::clone(&gate);
        let h = tokio::spawn(async move {
            g2.gate_tool(1, "write_file", json!({ "path": "x" }), "edit", None)
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
        assert!(matches!(h.await.unwrap(), Decision::Approve));
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
        assert!(!gate.modify("nonexistent-id", "instruction".into()).await);
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

        assert!(
            gate.approve_always(&id).await,
            "approve_always must resolve"
        );
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
        assert!(!gate.approve_always("no-such-id").await);
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
}
