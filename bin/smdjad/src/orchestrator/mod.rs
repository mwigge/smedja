//! Turn orchestration logic extracted from `run_turn` in `main.rs`.
//!
//! [`TurnOrchestrator`] encapsulates all the dependencies that were previously
//! threaded through the free function `run_turn` as parameters.  Call
//! [`TurnOrchestrator::run`] to execute a single agent turn end-to-end.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use opentelemetry::{
    global,
    trace::{Span as _, Status as SpanStatus, Tracer as _},
    KeyValue,
};
use smedja_adapter::types::{Message as AdapterMessage, Role as AdapterRole};
use smedja_adapter::CallOptions;
use smedja_assayer::{AgentRole, Assayer, Complexity, Route, Runner};
use smedja_bellows::event::CorrelationCtx;
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_ingot::{Checkpoint, CostEntry, IngotHandle, TokenSnapshot, TokensSavedEntry};
use smedja_memory::{estimate_messages_tokens, estimate_tokens, inject_conciseness, WorkingMemory};
use smedja_types::{Microdollars, Timestamp};
use smedja_vault::{Vault, VaultEntry};
use tokio::sync::Mutex;
use tracing::warn;
use uuid::Uuid;

use smedja_telemetry as tel;

use crate::cowork::{CoworkGate, Decision};
use crate::executor::execute_tool;
use crate::price_table::PriceTable;
use crate::provider_pool::ProviderPool;

pub(crate) mod cold;
use cold::VaultColdStore;

mod context;
use context::{classify_tool_outcome, cold_k_for_tier, model_context_window, strata_for_tier};

/// Shared map from session-resume keys to provider-native resume identifiers.
///
/// Constructed once in `main()` and threaded explicitly to every orchestrator
/// (replacing the former process-static `OnceLock` singleton) so tests can
/// supply their own map.
pub(crate) type ProviderSessions = Arc<Mutex<HashMap<String, String>>>;

/// Key identifying a persisted [`smedja_memory::CacheAligner`]: `(session_id, runner_name)`.
///
/// Keyed by runner as well as session because a [`smedja_memory::CacheHint`]
/// targets one specific provider's warm cache; a `provider-failover` runner
/// rotation must not smear one provider's prefix-digest history onto another.
pub(crate) type AlignerKey = (String, String);

/// Shared map from `(session_id, runner)` to its persisted cross-turn aligner.
///
/// Constructed once in `main()` and threaded to every orchestrator exactly like
/// [`ProviderSessions`], so a single aligner instance outlives an individual turn
/// and can observe the prior sealed prefix to report real `Grown`/`Mutated` drift.
pub(crate) type CacheAligners = Arc<Mutex<HashMap<AlignerKey, smedja_memory::CacheAligner>>>;

/// Builds the always-on foundational-discipline directive for the sealed system
/// prefix, gated by `config`.
///
/// TDD and clean-code discipline are steer-first: the directive is injected into
/// the cacheable system block on every code-writing turn so the agent is reminded
/// of the discipline every turn (the primary enforcement), with the diff backstop
/// secondary. Each discipline's clause is present only when its config flag is
/// `true`; when both are disabled the directive is omitted entirely (`None`).
#[must_use]
/// Language-aware variant: when `is_rust` is false, Rust-specific idioms are
/// replaced with generic equivalents so the directive is not actively misleading
/// in Python, TypeScript, or other workspaces.
pub(crate) fn methodology_directive_for(
    config: smedja_methodology::MethodologyConfig,
    is_rust: bool,
) -> Option<String> {
    if !config.tdd && !config.clean {
        return None;
    }
    let mut clauses: Vec<&'static str> = Vec::new();
    if config.tdd {
        clauses.push(
            "Write a failing test before the implementation it covers (Red, then Green, \
             then Refactor); keep functions small and focused; prefer an early return over \
             an `else` branch.",
        );
    }
    if config.clean {
        if is_rust {
            clauses.push(
                "Do not use `unwrap`, `expect`, or `println!` in library code — return errors \
                 with `?` and log through the structured logger.",
            );
        } else {
            clauses.push(
                "Do not swallow errors silently — propagate or log them explicitly. \
                 Avoid bare print statements in library code; use the structured logger.",
            );
        }
    }
    let body = clauses
        .iter()
        .map(|c| format!("- {c}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "<methodology_discipline>\n{body}\n</methodology_discipline>"
    ))
}

/// Derives a short title (≤10 words) from raw user turn content.
///
/// Strips any auto-injected context blocks (e.g. `<graph_symbols>`) that start
/// after a blank line and takes the first ten whitespace-separated words of the
/// remaining text.
pub(crate) fn derive_title(content: &str) -> String {
    let clean = content.split("\n\n<").next().unwrap_or(content).trim();
    clean
        .split_whitespace()
        .take(10)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Formats the current LSP snapshot into a `<lsp_diagnostics>` context block.
///
/// Returns `None` when there are no errors or warnings (info / hints are skipped).
/// At most 20 diagnostic lines are included; a trailing note is appended when
/// the list is truncated.
pub(crate) fn format_lsp_diagnostics(snapshot: &smedja_lsp::LspSnapshot) -> Option<String> {
    use smedja_lsp::types::Severity;
    const MAX: usize = 20;
    let relevant: Vec<_> = snapshot
        .diagnostics
        .iter()
        .filter(|d| matches!(d.severity, Severity::Error | Severity::Warning))
        .collect();
    if relevant.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = relevant
        .iter()
        .take(MAX)
        .map(|d| {
            let sev = match d.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                _ => "info",
            };
            let code = d
                .code
                .as_deref()
                .map_or_else(String::new, |c| format!(" [{c}]"));
            format!(
                "{} {}:{}: {}{}",
                sev,
                d.file.display(),
                d.line,
                d.message,
                code
            )
        })
        .collect();
    if relevant.len() > MAX {
        lines.push(format!(
            "... and {} more (only the first {MAX} shown)",
            relevant.len() - MAX
        ));
    }
    Some(format!(
        "<lsp_diagnostics>\n{}\n</lsp_diagnostics>",
        lines.join("\n")
    ))
}

/// Returns `true` when context fill exceeds the 80% auto-summarisation threshold.
pub(crate) fn context_pressure_exceeds_threshold(input_tokens: u32, context_window: usize) -> bool {
    if context_window == 0 {
        return false;
    }
    f64::from(input_tokens) / context_window as f64 >= 0.80
}

/// Builds the prompt sent to the LLM to produce a conversation summary.
///
/// At most 20 turns are included; older turns are dropped from the head.
pub(crate) fn build_summariser_prompt(history: &[(String, String)]) -> String {
    const MAX_TURNS: usize = 20;
    let turns: Vec<_> = history.iter().rev().take(MAX_TURNS).collect();
    let turns_text: String = turns
        .into_iter()
        .rev()
        .map(|(role, content)| format!("{role}: {content}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Produce a structured summary of the conversation so far. \
Format it as three clearly labelled sections using bullet points:\n\
- **Decisions**: key choices made and their rationale\n\
- **Changed files**: files created, edited, or deleted (with brief reason)\n\
- **Open questions**: unresolved issues or follow-up items\n\
Omit sections that have no content. Keep total length under 400 words.\n\n\
{turns_text}"
    )
}

/// Owns all the shared resources needed to execute a single agent turn.
pub(crate) struct TurnOrchestrator {
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    assayer: Arc<Assayer>,
    price_table: Arc<PriceTable>,
    vault: Arc<Mutex<Vault>>,
    embedder: Arc<dyn crate::embedder_port::Embedder>,
    provider_sessions: ProviderSessions,
    cache_aligners: CacheAligners,
    active_change: Option<String>,
    lsp_manager: Arc<smedja_lsp::LspManager>,
}

impl TurnOrchestrator {
    #[allow(clippy::too_many_arguments)] // forwarded directly from run_turn / loop runner
    pub(crate) fn new(
        ingot: IngotHandle,
        dispatcher: Arc<Dispatcher>,
        gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
        pool: Arc<ProviderPool>,
        assayer: Arc<Assayer>,
        price_table: Arc<PriceTable>,
        vault: Arc<Mutex<Vault>>,
        embedder: Arc<dyn crate::embedder_port::Embedder>,
        provider_sessions: ProviderSessions,
        cache_aligners: CacheAligners,
        active_change: Option<String>,
        lsp_manager: Arc<smedja_lsp::LspManager>,
    ) -> Self {
        Self {
            ingot,
            dispatcher,
            gates,
            pool,
            assayer,
            price_table,
            vault,
            embedder,
            provider_sessions,
            cache_aligners,
            active_change,
            lsp_manager,
        }
    }

    /// Execute a single agent turn: load task → route → call LLM → tool loop →
    /// persist response → checkpoint.
    ///
    /// All errors are handled internally; failures are published as
    /// [`TurnEvent::fail`] events and the task is marked `"failed"` in the
    /// ingot.  The function returns `()` rather than propagating, matching the
    /// existing `tokio::spawn` call sites.
    #[allow(clippy::too_many_lines)] // sequential turn pipeline kept inline to preserve a single tracing span scope
    pub(crate) async fn run(self, session_id: String, turn_id: String) {
        const MAX_RATE_LIMIT_RETRIES: u32 = 4;
        const RATE_LIMIT_BACKOFF_BASE_SECS: u64 = 1;
        // A turn rotates to at most this many alternative providers (4 providers
        // total including the routed one) before failing, independent of the
        // per-provider rate-limit back-off budget.
        const MAX_PROVIDER_ROTATIONS: u32 = 3;
        // Cold context is bounded to at most this fraction (1/N) of the tier
        // token budget so recalled context can never displace hot turns.
        const COLD_BUDGET_DIVISOR: usize = 4;

        let ingot = &self.ingot;
        let dispatcher = &self.dispatcher;
        let gates = &self.gates;
        let pool = &self.pool;
        let assayer = &self.assayer;
        let price_table = &self.price_table;
        let vault = &self.vault;
        let embedder = &self.embedder;
        let provider_sessions = &self.provider_sessions;
        let cache_aligners = &self.cache_aligners;

        let tracer = global::tracer("smedja");
        let mut turn_span = tracer.start(tel::SPAN_AGENT_INVOKE);

        // 1. Load the task to retrieve user content.
        let task = {
            match ingot.get_task(&turn_id).await {
                Ok(Some(t)) => t,
                Ok(None) => {
                    warn!(turn_id = %turn_id, "task not found; dropping turn");
                    dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, "task not found"));
                    turn_span.set_status(SpanStatus::error("task not found"));
                    turn_span.end();
                    return;
                }
                Err(e) => {
                    warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to load task");
                    let reason = e.to_string();
                    dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
                    turn_span.set_status(SpanStatus::error(reason));
                    turn_span.end();
                    return;
                }
            }
        };

        // 2. Route this turn to a provider via the assayer.
        //    Role comes from session.mode; complexity is conservatively Coding for now.
        // Active role from session.mode — hoisted so it can also drive role-bound
        // skill injection further down.
        let session_mode = {
            ingot
                .get_session(&session_id)
                .await
                .ok()
                .flatten()
                .and_then(|s| s.mode)
        };
        let role = session_mode
            .as_deref()
            .and_then(crate::common::parse_session_mode_to_role)
            .unwrap_or(AgentRole::Orchestrator);
        let route = {
            let complexity = match role {
                AgentRole::Ask | AgentRole::Search => Complexity::Simple,
                AgentRole::Impl | AgentRole::Test | AgentRole::Debug => Complexity::Coding,
                _ => Complexity::Complex,
            };
            let decision = assayer.route_decision(role, complexity);
            tracing::debug!(
                turn_id = %turn_id,
                role = ?role,
                complexity = ?decision.complexity(),
                rationale = %decision.rationale(),
                "routing turn"
            );
            Route {
                runner: decision.runner(),
                tier: decision.tier(),
                model: decision.model().map(str::to_owned),
                tools: vec![],
            }
        };

        // Apply session runner override.
        let route = {
            let override_runner = {
                ingot
                    .get_session(&session_id)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|s| s.runner_override)
                    .and_then(|r| crate::common::parse_runner_str(&r))
            };
            if let Some(overridden) = override_runner {
                Route {
                    runner: overridden,
                    ..route
                }
            } else {
                route
            }
        };

        // Build the ordered rotation ring of eligible providers for this route.
        // The turn is driven over the ring; a retryable failure advances to the
        // next entry (bounded by MAX_PROVIDER_ROTATIONS).
        let ring = pool.eligible_ring(route.runner, route.tier);
        if ring.is_empty() {
            let reason = "no LLM provider available; turn cannot execute".to_owned();
            warn!(session_id = %session_id, turn_id = %turn_id, "{reason}");
            let _ = ingot.update_task_status(&turn_id, "failed").await;
            dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
            turn_span.set_status(SpanStatus::error(reason));
            turn_span.end();
            return;
        }

        // 3. Load session for workspace root, cowork mode, and task context.
        let session = { ingot.get_session(&session_id).await.ok().flatten() };

        turn_span.set_attribute(KeyValue::new(tel::CONV_ID, session_id.clone()));
        turn_span.set_attribute(KeyValue::new(
            tel::OPERATION_NAME,
            tel::OPERATION_INVOKE_AGENT,
        ));
        turn_span.set_attribute(KeyValue::new(tel::SESSION_ID, session_id.clone()));
        turn_span.set_attribute(KeyValue::new(tel::TURN_ID, turn_id.clone()));
        turn_span.set_attribute(KeyValue::new(
            tel::AGENT_NAME,
            session
                .as_ref()
                .and_then(|s| s.mode.as_deref())
                .unwrap_or("interactive")
                .to_owned(),
        ));

        let workspace_root = {
            let p = session
                .as_ref()
                .and_then(|s| s.workspace_root.as_deref())
                .map_or_else(
                    || {
                        std::env::var("SMEDJA_WORKSPACE")
                            .map_or_else(|_| PathBuf::from("."), PathBuf::from)
                    },
                    PathBuf::from,
                );
            if !p.join(".git").exists() {
                tracing::warn!(
                    path = %p.display(),
                    "workspace does not contain .git; tool execution may be in wrong directory",
                );
            }
            p
        };

        let task_prefix = {
            match ingot.get_session(&session_id).await {
                Ok(Some(s)) => {
                    if let Some(ref task_id) = s.task_id {
                        match ingot.get_task(task_id).await {
                            Ok(Some(active_task)) => format!(
                                "\n\n<active_task>\n<title>{}</title>\n<description>{}</description>\n</active_task>",
                                crate::common::xml_escape(&active_task.title),
                                crate::common::xml_escape(active_task.description.as_str()),
                            ),
                            _ => String::new(),
                        }
                    } else {
                        String::new()
                    }
                }
                _ => String::new(),
            }
        };

        // Base system prompt, with workspace skills folded into the stable
        // (cacheable) system block. `base_system` is kept unsteered so verbosity
        // steering can be re-applied per tool-loop iteration without compounding.
        let base_system = {
            let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
            let base = format!(
                "You are smedja, an AI coding assistant.\
                \nWorkspace: {workspace_root}\
                \nDate: {today}{task_prefix}\
                \n\nBe concise and direct. Apply the smallest diff that satisfies a \
                request. Prefer reading graph/vault context before opening files, and \
                reading files before writing them. When <recalled_context>, \
                <cold_context>, or <graph_symbols> blocks are present, treat them as \
                authoritative — reference specifics from them rather than asking the \
                user to repeat information. Ask before acting only when the request is \
                genuinely ambiguous or would be destructive.",
                workspace_root = workspace_root.display(),
            );
            let with_skills = match smedja_memory::load_workspace_skills(&workspace_root) {
                Ok(skills) if !skills.is_empty() => {
                    let joined = skills.join("\n\n");
                    format!("{base}\n\n<workspace_skills>\n{joined}\n</workspace_skills>")
                }
                Ok(_) => base,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to load workspace skills; continuing without");
                    base
                }
            };
            // Role-bound rules/skills: inject the active role's pack
            // (`.smedja/roles/<role>.md` / `roles/<role>/*.md`) so each role
            // carries its own discipline (e.g. review checklist, research
            // source-hygiene, planning rules).
            let with_skills = match smedja_memory::load_role_skills(&workspace_root, role.label()) {
                Ok(role_skills) if !role_skills.is_empty() => {
                    let joined = role_skills.join("\n\n");
                    format!(
                        "{with_skills}\n\n<role_skills role=\"{}\">\n{joined}\n</role_skills>",
                        role.label()
                    )
                }
                Ok(_) => with_skills,
                Err(e) => {
                    tracing::warn!(error = %e, role = role.label(), "failed to load role skills; continuing without");
                    with_skills
                }
            };
            // Project-specific context files from `.smedja/context/*.md` are
            // injected here so they ride the stable (cacheable) system block.
            let with_skills = match smedja_memory::load_context_files(&workspace_root) {
                Ok(files) if !files.is_empty() => {
                    let joined = files.join("\n\n");
                    format!("{with_skills}\n\n<project_context>\n{joined}\n</project_context>")
                }
                Ok(_) => with_skills,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to load context files; continuing without");
                    with_skills
                }
            };
            // Always-on, steer-first foundational discipline: the directive is
            // folded into the same cacheable system block as workspace skills so
            // it is sealed into the stable prefix before `seal_prefix()` and the
            // agent is reminded of the discipline on every code-writing turn.
            // Config-gated per discipline; omitted entirely when both are off.
            let methodology_config =
                crate::methodology_config::load_methodology_config(&workspace_root);
            let is_rust_workspace = workspace_root.join("Cargo.toml").exists();
            match methodology_directive_for(methodology_config, is_rust_workspace) {
                Some(directive) => format!("{with_skills}\n\n{directive}"),
                None => with_skills,
            }
        };

        let mcp_tools: Vec<serde_json::Value> = {
            ingot
                .list_mcp_servers()
                .await
                .unwrap_or_default()
                .into_iter()
                .flat_map(
                    |s| match serde_json::from_str::<Vec<serde_json::Value>>(&s.tools_json) {
                        Ok(tools) => tools,
                        Err(e) => {
                            tracing::warn!(
                                server = %s.name,
                                error = %e,
                                "failed to deserialize MCP tools_json; skipping server"
                            );
                            vec![]
                        }
                    },
                )
                .collect()
        };
        if !mcp_tools.is_empty() {
            tracing::debug!(count = mcp_tools.len(), "injecting MCP tools into turn");
        }

        let local_tool_format = {
            let local_base = std::env::var("SMEDJA_LOCAL_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:8080".to_owned());
            let health_arc = crate::local_provider::global_health();
            let needs_recheck = {
                let h = health_arc.lock().await;
                (crate::common::now_epoch() - h.last_checked) > 30.0
            };
            if needs_recheck {
                let fresh = crate::local_provider::check_health(&local_base).await;
                let fmt = fresh.tool_format.clone();
                *health_arc.lock().await = fresh;
                fmt
            } else {
                health_arc.lock().await.tool_format.clone()
            }
        };
        if local_tool_format == "xml" {
            tracing::debug!(tool_format = "xml", "local provider tool format: xml");
        }

        let mut builtin_tools: Vec<serde_json::Value> = vec![
            serde_json::json!({
                "name": "smedja_vault_search",
                "description": "Search the smedja vault for semantically similar entries. \
                    namespace: optional — defaults to 'default'; use 'compact' for session \
                    summaries, or the role label (e.g. 'review', 'sre') for role-scoped knowledge. \
                    k: number of results to return, default 3.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "namespace": { "type": "string", "description": "defaults to 'default'; known values: compact, default, review, sre, researcher" },
                        "k": { "type": "integer", "description": "number of results, default 3" }
                    },
                    "required": ["query"]
                }
            }),
            serde_json::json!({
                "name": "smedja_vault_store",
                "description": "Store an entry in the smedja vault for future retrieval. \
                    namespace: optional — defaults to 'default'; use 'compact' for session \
                    summaries, or the role label (e.g. 'review', 'sre') for role-scoped knowledge. \
                    Omitting namespace stores in 'default', which is always included in proactive recall.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string" },
                        "namespace": { "type": "string", "description": "defaults to 'default'; known values: compact, default, review, sre, researcher" },
                        "id": { "type": "string" },
                        "payload": { "type": "object" },
                        "source_file": { "type": "string" },
                        "added_by": { "type": "string" }
                    },
                    "required": ["content"]
                }
            }),
            serde_json::json!({
                "name": "smedja_retrieve",
                "description": "Retrieve the original full content for a compressed block by its content hash.",
                "input_schema": {
                    "type": "object",
                    "properties": { "hash": { "type": "string" } },
                    "required": ["hash"]
                }
            }),
            serde_json::json!({
                "name": "graph_query",
                "description": "Query the workspace code graph for symbols related to a query.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "depth": { "type": "integer" }
                    },
                    "required": ["query"]
                }
            }),
        ];
        let is_sre_mode = session
            .as_ref()
            .and_then(|s| s.mode.as_deref())
            .is_some_and(|m| m == "sre");
        if is_sre_mode {
            builtin_tools.push(serde_json::json!({
                "name": "alert_list",
                "description": "Drain up to 50 pending alerts from the alert queue.",
                "input_schema": { "type": "object", "properties": {} }
            }));
            builtin_tools.push(serde_json::json!({
                "name": "otel_query",
                "description": "Query SigNoz traces API.",
                "input_schema": { "type": "object", "properties": { "service": { "type": "string" }, "filter": { "type": "string" }, "range_minutes": { "type": "integer" } }, "required": ["service"] }
            }));
            builtin_tools.push(serde_json::json!({
                "name": "metric_query",
                "description": "Query Prometheus with PromQL.",
                "input_schema": { "type": "object", "properties": { "promql": { "type": "string" }, "range_minutes": { "type": "integer" } }, "required": ["promql"] }
            }));
            builtin_tools.push(serde_json::json!({
                "name": "log_tail",
                "description": "Tail logs from Loki.",
                "input_schema": { "type": "object", "properties": { "service": { "type": "string" }, "filter": { "type": "string" }, "lines": { "type": "integer" } }, "required": ["service"] }
            }));
        }

        let all_tools: Vec<serde_json::Value> =
            builtin_tools.into_iter().chain(mcp_tools).collect();

        // Per-runner-tier strata + token budget. `fast` keeps a shallow warm
        // window and small budget; `deep` keeps the full warm window and a large
        // budget; `local` sits between. The budget caps the warm stratum — the
        // stable prefix and hot turns are always included verbatim.
        let (strata, budget_tokens) = strata_for_tier(route.tier);

        // 4. Assemble the stable prefix (the user turn plus auto-injected graph
        //    symbols) into working memory, then seal it so the prefix survives
        //    the tool loop unchanged and drives the provider KV-cache hint.
        let cold_adapter = Arc::new(VaultColdStore::new(Arc::clone(vault), Arc::clone(embedder)));
        let mut mem = WorkingMemory::new(budget_tokens).with_cold_store(cold_adapter);
        mem.set_strata(strata);
        // Cold recall scales with tier depth: fast favours latency (k=1), deep
        // favours recall (k=5). Query all three relevant namespaces: session
        // summaries ("compact"), user notes ("default"), and the active role's
        // knowledge store (e.g. "review", "sre") when a non-default role is set.
        // set_cold_query is called per namespace; results are accumulated before
        // assemble_cold_block applies the budget cap.
        let cold_k = cold_k_for_tier(route.tier);
        mem.set_cold_query("compact", cold_k);

        let first_user_content = {
            let mut content = task.title.clone();
            // Auto-inject top-3 graph symbols related to user message nouns.
            let stop_words = [
                "the", "and", "for", "with", "this", "that", "from", "into", "use", "are", "was",
                "has", "not", "can", "its", "will",
            ];
            let nouns: Vec<&str> = task
                .title
                .split_whitespace()
                .filter(|t| t.len() >= 3 && !stop_words.contains(&t.to_lowercase().as_str()))
                .take(5)
                .collect();
            let mut injected_count = 0usize;
            if !nouns.is_empty() {
                let graph_db_path = crate::handlers::graph::graph_db_path(&workspace_root);
                if graph_db_path.exists() {
                    match smedja_graph::GraphStore::open(&graph_db_path) {
                        Ok(store) => {
                            let query = nouns.join(" ");
                            match store.graph_query(&query, 3, 2) {
                                Ok(symbols) => {
                                    if !symbols.is_empty() {
                                        let snippets: Vec<String> = symbols
                                            .iter()
                                            .map(|s| {
                                                format!(
                                                    "// {} {} ({}:{})\n{}",
                                                    s.kind.as_str(),
                                                    s.name,
                                                    s.file_path,
                                                    s.start_line,
                                                    s.snippet
                                                )
                                            })
                                            .collect();
                                        let _ = write!(
                                            content,
                                            "\n\n<graph_symbols>\n{}\n</graph_symbols>",
                                            snippets.join("\n\n")
                                        );
                                        injected_count = symbols.len();
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!(error = %e, "graph_query failed; skipping injection");
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "could not open graph.db; skipping injection");
                        }
                    }
                } else {
                    tracing::debug!("graph.db not found; skipping auto-injection");
                }
            }
            tracing::debug!(
                smedja.turn.graph_symbols_injected = injected_count,
                "graph symbol injection"
            );
            // Append LSP diagnostics only when the turn is code-focused: coder
            // roles or queries that mention fix/build/compile/error keywords.
            let wants_diag = matches!(role, AgentRole::Impl | AgentRole::Debug | AgentRole::Test)
                || ["fix", "build", "compile", "error", "warn"]
                    .iter()
                    .any(|kw| task.title.to_lowercase().contains(kw));
            if wants_diag {
                if let Some(diag_block) = format_lsp_diagnostics(&self.lsp_manager.snapshot()) {
                    let _ = write!(content, "\n\n{diag_block}");
                }
            }
            content
        };

        // Proactive vault recall: search the "default" namespace for entries
        // semantically similar to the user's query. The block is emitted as a
        // system-role message (alongside cold_context) rather than inside the
        // user message, so provider attention weighting treats it as context
        // rather than user utterance, and the user message stays clean.
        let vault_recall_block: Option<String> = {
            let q = embedder.embed_query(&task.title).await;
            let mid = embedder.model_id().to_owned();
            let d = embedder.dim();
            let v = Arc::clone(vault);
            let t = task.title.clone();
            let entries = tokio::task::spawn_blocking(move || {
                v.blocking_lock()
                    .search(&q, &t, "default", 3, &mid, d)
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default();
            tracing::debug!(smedja.turn.vault_recalled = entries.len(), "vault recall");
            format_vault_recalled(&entries)
        };

        // 4a. Cold recall: pull semantically-relevant context from beyond the
        //     warm window and inject it as a single delimited system block ahead
        //     of the user turn, so it falls inside the sealed prefix. The block
        //     is capped at a fraction of the tier budget; lowest-scored entries
        //     are dropped until it fits, so cold context never displaces hot
        //     turns.
        let mut cold_results = mem.cold_context(&task.title).await;
        // Also recall from user-stored notes and the active role's namespace.
        mem.set_cold_query("default", cold_k.min(3));
        cold_results.extend(mem.cold_context(&task.title).await);
        if role != AgentRole::Orchestrator {
            mem.set_cold_query(role.label(), cold_k.min(3));
            cold_results.extend(mem.cold_context(&task.title).await);
        }
        let cold_budget = budget_tokens / COLD_BUDGET_DIVISOR;
        let cold_injected = match cold::assemble_cold_block(&cold_results, cold_budget) {
            Some((block, count)) => {
                mem.push(block);
                count
            }
            None => 0,
        };
        // Push proactive vault recall as a system message alongside cold_context.
        if let Some(block) = vault_recall_block {
            mem.push(AdapterMessage::system(block));
        }
        tracing::debug!(
            smedja.turn.cold_results_injected = cold_injected,
            "cold context injection"
        );

        // Replay prior completed turns for this session so the provider has the
        // full conversation context. smedja used to delegate memory to the CLI's
        // `--resume`, but that is environment-fragile and was dropped; loading
        // the history here (milliways-style) makes multi-turn work everywhere.
        match ingot.session_history(&session_id).await {
            Ok(history) => {
                for past in history {
                    if past.id.to_string() == turn_id {
                        continue; // never replay the in-flight turn
                    }
                    mem.push(AdapterMessage::user(past.title));
                    if let Some(resp) = past.response {
                        if !resp.is_empty() {
                            mem.push(AdapterMessage::assistant(resp));
                        }
                    }
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "could not load session history; continuing without it");
            }
        }

        mem.push(AdapterMessage::user(first_user_content));
        mem.seal_prefix();

        // The sealed prefix is observed for cross-turn drift *inside the ring
        // loop*, where the runner name is known: the aligner is persisted per
        // `(session_id, runner)`, so the hint must be computed against the
        // runner's own prefix history. See the `cache_aligners` lookup below.

        // 4b. Mark in_progress.
        {
            if let Err(e) = ingot.update_task_status(&turn_id, "in_progress").await {
                warn!(turn_id = %turn_id, error = %e, "failed to mark task in_progress");
            }
        }

        let mut full_response = String::new();
        let mut total_input_tokens = 0u32;
        let mut total_output_tokens = 0u32;
        // Provider-reported cache reads ("input not re-paid") and the estimated
        // cold-context tokens dropped from the prompt — both accumulated across
        // the tool loop and recorded once per turn as source-tagged savings.
        let mut total_cache_read_tokens = 0u32;
        let mut total_cold_omitted_tokens = 0usize;

        // Correlation for streamed events: carry the turn span's trace/span ids
        // so deltas and tool events are linkable to the turn, not just Started.
        let turn_correlation = {
            let sc = turn_span.span_context();
            if sc.is_valid() {
                CorrelationCtx {
                    trace_id: Some(sc.trace_id().to_string()),
                    span_id: Some(sc.span_id().to_string()),
                    conversation_id: Some(session_id.clone()),
                    ..CorrelationCtx::default()
                }
            } else {
                CorrelationCtx {
                    conversation_id: Some(session_id.clone()),
                    ..CorrelationCtx::default()
                }
            }
        };

        // The runner-name and model of the attempt that ultimately produced the
        // turn's output; updated on each rotation so cost/checkpoint records
        // reflect the provider that actually served the turn.
        let mut runner = String::new();
        let mut model = String::new();
        // The most recent classified failure kind, used as the terminal failure
        // reason when the ring is exhausted.
        let mut last_kind: &'static str = "request";
        let mut rotations: u32 = 0;

        // Wall-clock deadline shared across all provider rotations and tool-loop
        // iterations for this turn.  Using a single `Instant` deadline (rather
        // than a fresh per-iteration duration) prevents the effective ceiling from
        // multiplying up to `MAX_TOOL_TURNS * 5 min`.
        let turn_deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(crate::common::effective_agent_timeout_s());

        // Drive the turn over the eligible ring. On a retryable failure the loop
        // advances to the next entry (bounded by MAX_PROVIDER_ROTATIONS),
        // re-deriving CallOptions for the new provider while preserving the same
        // WorkingMemory prompt and accumulated tool history.
        let mut turn_context_window: usize = 128_000;
        'ring: for entry in &ring {
            let entry_runner_name = entry.runner_name.to_owned();
            let runner_enum = entry.runner;
            // Key the provider-native resume id by (session, runner), NOT by
            // runner alone. A bare runner key ("claude-cli") is global: the
            // first turn's native conversation id leaks into every other
            // session, so later turns pass `--resume <stale id>` and the CLI
            // fails with "No conversation found" (exit 1). Scoping by session id
            // means a fresh session starts with no resume and each session keeps
            // its own conversation.
            let session_store_key = format!(
                "{session_id}:{}",
                crate::common::runner_session_key(runner_enum)
            );

            // Re-derive the model for this entry: explicit route/env/session
            // override take precedence over the entry's default model.
            let entry_model = route
                .model
                .clone()
                .or_else(|| std::env::var("SMEDJA_MODEL").ok())
                .unwrap_or_else(|| entry.default_model.clone());
            let entry_model = session
                .as_ref()
                .and_then(|s| s.model_override.clone())
                .unwrap_or(entry_model);
            let context_window = model_context_window(&entry_model);
            turn_context_window = context_window;

            turn_span.set_attribute(KeyValue::new(tel::GEN_AI_SYSTEM, entry_runner_name.clone()));
            turn_span.set_attribute(KeyValue::new(tel::REQUEST_MODEL, entry_model.clone()));

            // Resolve the provider-native resume id from the NEW runner's session
            // key; a resume id from a previously-failed runner is never carried
            // across providers.
            let provider_session_id = if matches!(runner_enum, Runner::Claude | Runner::Codex) {
                provider_sessions
                    .lock()
                    .await
                    .get(&session_store_key)
                    .cloned()
            } else {
                None
            };

            // Observe the sealed prefix for cross-turn drift using the aligner
            // persisted for THIS runner. Keying by `(session_id, runner)` keeps
            // each provider's prefix history separate: a `provider-failover`
            // rotation to a new runner finds no entry and starts fresh (its cache
            // is cold), and rotating back resumes that runner's recorded boundary.
            // The lock is held only for the take/align/re-insert and released
            // before the provider call below (no lock across `.await`).
            let aligner_key: AlignerKey = (session_id.clone(), entry_runner_name.clone());
            let cache_hint = {
                let mut aligners = cache_aligners.lock().await;
                let mut aligner = aligners.remove(&aligner_key).unwrap_or_default();
                let hint = aligner.align(&mem);
                aligners.insert(aligner_key, aligner);
                hint
            };

            // Realise the aligner hint for this runner: Anthropic via
            // `stable_prefix_len` (unchanged), OpenAI via stable-prefix ordering
            // plus a per-session cache key, Gemini via an optional context-cache
            // handle (lifecycle out of scope — none is supplied here, so Gemini
            // falls back to plain contents). Providers without prompt caching get
            // no hint.
            let openai_cache_key = (entry_runner_name == "openai").then(|| session_id.clone());
            let (stable_prefix_len, cache_strategy) = context::cache_options_for_runner(
                &entry_runner_name,
                cache_hint,
                openai_cache_key,
                None,
            );

            // The session's permission mode (default Ask), threaded so external
            // CLIs that can't gate per-tool (codex) can still map it to a
            // sandbox level.
            let perm_mode = {
                let gate = gates.lock().await.get(&session_id).cloned();
                match gate {
                    Some(g) => g.mode().await.as_str().to_owned(),
                    None => crate::cowork::PermissionMode::default().as_str().to_owned(),
                }
            };

            let mut opts = CallOptions {
                model: entry_model.clone(),
                max_tokens: Some(2048),
                temperature: Some(0.7),
                system: Some(base_system.clone()),
                tools: if all_tools.is_empty() {
                    None
                } else {
                    Some(all_tools.clone())
                },
                provider_session_id,
                smedja_session_id: Some(session_id.clone()),
                permission_mode: Some(perm_mode),
                stable_prefix_len,
                cache_strategy,
                workspace: Some(workspace_root.clone()),
            };

            runner = entry_runner_name.clone();
            model = entry_model.clone();
            let provider = &entry.provider;

            // Classified failure that triggers a rotation to the next ring entry.
            // `None` means the attempt completed (success or fatal handled inline).
            let mut rotate: Option<(&'static str, String)> = None;

            'tool_loop: for _iteration in 0..crate::common::effective_max_tool_turns() {
                // 5a. Stream LLM response with rate-limit retry.
                let (
                    response_text,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    native_session_id,
                ) = {
                    let mut backoff_secs = RATE_LIMIT_BACKOFF_BASE_SECS;
                    let mut attempt = 0u32;
                    // Assemble the budgeted prompt and apply verbosity steering for
                    // this iteration. The prompt always leads with the sealed prefix
                    // and all hot turns; warm turns are included until the budget is
                    // spent. The conciseness directive is appended above 60% fill.
                    let (prompt, omitted) = mem.build_prompt_with_omitted(budget_tokens);
                    total_cold_omitted_tokens = total_cold_omitted_tokens.saturating_add(omitted);
                    let used = estimate_messages_tokens(&prompt)
                        + estimate_tokens(opts.system.as_deref().unwrap_or(""));
                    opts.system = Some(inject_conciseness(&base_system, used, context_window));
                    loop {
                        let stream = provider.stream_chat(&prompt, &opts);
                        let drain_result = tokio::time::timeout_at(
                            turn_deadline,
                            crate::common::drain_stream(
                                stream,
                                dispatcher,
                                Some(turn_id.as_str()),
                                &turn_correlation,
                            ),
                        )
                        .await;
                        match drain_result {
                            Ok(Ok(triple)) => break triple,
                            Ok(Err(crate::common::DrainError::RateLimited { retry_after })) => {
                                attempt += 1;
                                if attempt > MAX_RATE_LIMIT_RETRIES {
                                    // Back-off budget spent: escalate to rotation
                                    // rather than failing the turn.
                                    warn!(turn_id = %turn_id, "rate limit retry limit exceeded; rotating");
                                    rotate = Some((
                                        "rate_limited",
                                        "rate limited by provider; retry limit exceeded".to_owned(),
                                    ));
                                    break 'tool_loop;
                                }
                                let sleep_secs =
                                    retry_after.map_or(backoff_secs, |d| d.as_secs().max(1));
                                warn!(
                                    turn_id = %turn_id,
                                    attempt,
                                    sleep_secs,
                                    "rate limited by provider; backing off"
                                );
                                tokio::time::sleep(std::time::Duration::from_secs(sleep_secs))
                                    .await;
                                backoff_secs = (backoff_secs * 2).min(60);
                            }
                            Ok(Err(crate::common::DrainError::Rotatable { kind, .. })) => {
                                rotate =
                                    Some((kind, format!("rotatable provider failure: {kind}")));
                                break 'tool_loop;
                            }
                            Ok(Err(crate::common::DrainError::Other(reason))) => {
                                warn!(turn_id = %turn_id, error = %reason, "stream error during turn");
                                let _ = ingot.update_task_status(&turn_id, "failed").await;
                                dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
                                tel::set_span_error(&mut turn_span, "request", &reason, false);
                                turn_span.end();
                                return;
                            }
                            Err(_elapsed) => {
                                let reason = format!(
                                    "turn deadline exceeded after {}s",
                                    crate::common::effective_agent_timeout_s()
                                );
                                warn!(turn_id = %turn_id, "turn wall-clock deadline exceeded");
                                let _ = ingot.update_task_status(&turn_id, "failed").await;
                                dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
                                tel::set_span_error(&mut turn_span, "request", &reason, false);
                                turn_span.end();
                                return;
                            }
                        }
                    }
                };
                if matches!(runner_enum, Runner::Claude | Runner::Codex) {
                    if let Some(native_session_id) = native_session_id {
                        provider_sessions
                            .lock()
                            .await
                            .insert(session_store_key.clone(), native_session_id);
                    }
                }
                total_input_tokens = total_input_tokens.saturating_add(input_tokens);
                total_output_tokens = total_output_tokens.saturating_add(output_tokens);
                total_cache_read_tokens = total_cache_read_tokens.saturating_add(cache_read_tokens);

                // 5b. Parse tool calls from the response text.
                let tool_call = crate::executor::parse_tool_call(&response_text);

                if let Some((tool_name, mut tool_input)) = tool_call {
                    let tool_call_id = Uuid::new_v4().to_string();

                    mem.push(AdapterMessage::assistant(response_text.clone()));

                    let (ev_trace_id, ev_span_id) = {
                        use opentelemetry::trace::TraceContextExt as _;
                        let cx = opentelemetry::Context::current();
                        let sc = cx.span().span_context().clone();
                        if sc.is_valid() {
                            (
                                Some(format!("{}", sc.trace_id())),
                                Some(format!("{}", sc.span_id())),
                            )
                        } else {
                            (None, None)
                        }
                    };
                    dispatcher.publish(TurnEvent::ToolCalled {
                        tool_name: tool_name.clone(),
                        input_summary: tool_input.chars().take(120).collect(),
                        full_input: Some(tool_input.chars().take(4096).collect()),
                        turn_id: Some(turn_id.clone()),
                        correlation: CorrelationCtx {
                            conversation_id: Some(session_id.clone()),
                            trace_id: ev_trace_id,
                            span_id: ev_span_id,
                            parent_span_id: None,
                            operation_name: Some(tel::OPERATION_EXECUTE_TOOL.to_owned()),
                            agent_name: session
                                .as_ref()
                                .and_then(|s| s.mode.as_deref())
                                .map(str::to_owned),
                            status: None,
                        },
                        tool_call_id: Some(tool_call_id.clone()),
                    });

                    // 5c. Permission gate. The session's permission policy
                    // (default Ask) decides allow/deny outright or — for a
                    // mutation under Ask — suspends until the user responds via
                    // cowork.approve/deny (surfaced by the TUI's cowork.pending
                    // poll). Ask-on-mutation is the default; Auto/Plan/
                    // AcceptEdits are toggled from the TUI (cowork.set_mode).
                    // The gate is created on demand so gating works without an
                    // explicit `/cowork on`.
                    let cowork_denied = if role.is_read_only()
                        && crate::cowork::evaluate(crate::cowork::PermissionMode::Plan, &tool_name)
                            == crate::cowork::PermissionDecision::Deny
                    {
                        // Read-only roles (plan/research/review/ask/orchestrator)
                        // can never mutate, regardless of the permission mode.
                        Some(format!(
                            "denied: the {} role is read-only and cannot run {tool_name}",
                            role.label()
                        ))
                    } else {
                        let gate = {
                            let mut g = gates.lock().await;
                            Arc::clone(
                                g.entry(session_id.clone())
                                    .or_insert_with(|| Arc::new(CoworkGate::default())),
                            )
                        };
                        let args_scrubbed =
                            serde_json::from_str(&tool_input).unwrap_or(serde_json::Value::Null);
                        // High-risk roles (IaC) always confirm a mutation — never
                        // auto-approved even in Auto/AcceptEdits — because the ops
                        // (apply/destroy) are dangerous and hard to reverse.
                        let push = Some((dispatcher.as_ref(), Some(turn_id.as_str())));
                        let decision = if role.is_high_risk()
                            && crate::cowork::evaluate(
                                crate::cowork::PermissionMode::Plan,
                                &tool_name,
                            ) == crate::cowork::PermissionDecision::Deny
                        {
                            gate.gate_tool_forced_ask(0, &tool_name, args_scrubbed, "", push)
                                .await
                        } else {
                            gate.gate_tool(0, &tool_name, args_scrubbed, "", push).await
                        };
                        match decision {
                            Decision::Approve => None,
                            Decision::Deny(reason) => Some(format!("denied: {reason}")),
                            Decision::Modify(new_input) => {
                                tool_input = new_input;
                                None
                            }
                        }
                    };

                    // 5d. Execute the tool (or return the denial).
                    let tool_result = if let Some(denial) = cowork_denied {
                        denial
                    } else {
                        let tool_type_val =
                            if crate::executor::LOCAL_TOOLS.contains(&tool_name.as_str()) {
                                if matches!(
                                    tool_name.as_str(),
                                    "smedja_vault_search" | "smedja_vault_store" | "graph_query"
                                ) {
                                    "datastore"
                                } else {
                                    "function"
                                }
                            } else {
                                "extension"
                            };
                        let mut tool_span = tracer.start(tel::SPAN_TOOL_EXECUTE);
                        tool_span.set_attribute(KeyValue::new(
                            tel::OPERATION_NAME,
                            tel::OPERATION_EXECUTE_TOOL,
                        ));
                        tool_span.set_attribute(KeyValue::new(tel::TOOL_NAME, tool_name.clone()));
                        tool_span.set_attribute(KeyValue::new(tel::TOOL_TYPE, tool_type_val));
                        tool_span
                            .set_attribute(KeyValue::new(tel::TOOL_CALL_ID, tool_call_id.clone()));
                        match tel::tool_args_capture_mode() {
                            tel::CaptureMode::Hash => {
                                tool_span.set_attribute(KeyValue::new(
                                    tel::TOOL_ARGS_HASH,
                                    tel::content_hash(&tool_input),
                                ));
                            }
                            tel::CaptureMode::Scrubbed | tel::CaptureMode::Full => {
                                tool_span.set_attribute(KeyValue::new(
                                    tel::TOOL_ARGS_HASH,
                                    tel::content_hash(&tool_input),
                                ));
                                tool_span.set_attribute(KeyValue::new(
                                    "gen_ai.tool.args",
                                    tel::scrub_and_summarise(&tool_input),
                                ));
                            }
                        }
                        let result = execute_tool(
                            &tool_name,
                            &tool_input,
                            &workspace_root,
                            session.as_ref(),
                            ingot,
                            vault,
                            embedder,
                        )
                        .await;
                        tool_span.set_attribute(KeyValue::new(
                            tel::TOOL_RESULT_HASH,
                            tel::content_hash(&result),
                        ));
                        tool_span.set_attribute(KeyValue::new(
                            tel::TOOL_RESULT_TOKENS,
                            i64::try_from(result.split_whitespace().count()).unwrap_or(0),
                        ));
                        let outcome = classify_tool_outcome(&result);
                        if outcome.is_success() {
                            use opentelemetry::trace::Span as _;
                            tool_span.set_status(opentelemetry::trace::Status::Ok);
                        } else {
                            use opentelemetry::trace::Span as _;
                            tool_span.set_status(opentelemetry::trace::Status::error(
                                result.chars().take(120).collect::<String>(),
                            ));
                        }
                        tool_span.end();
                        result
                    };

                    // Persist tool execution as a timeline event.
                    {
                        let tool_sc = {
                            use opentelemetry::trace::TraceContextExt as _;
                            let cx = opentelemetry::Context::current();
                            cx.span().span_context().clone()
                        };
                        let (t_trace_id, t_span_id) = if tool_sc.is_valid() {
                            (
                                Some(format!("{}", tool_sc.trace_id())),
                                Some(format!("{}", tool_sc.span_id())),
                            )
                        } else {
                            (None, None)
                        };
                        let tool_audit = smedja_ingot::AuditEvent {
                            id: Uuid::new_v4(),
                            ts: Timestamp::now(),
                            session_id: session_id.clone(),
                            turn_id: Some(turn_id.clone()),
                            action_type: "tool_exec".into(),
                            actor: "smdjad".into(),
                            tool_name: Some(tool_name.clone()),
                            traceparent: None,
                            trace_id: t_trace_id,
                            span_id: t_span_id,
                            conversation_id: Some(session_id.clone()),
                            tool_call_id: Some(tool_call_id.clone()),
                            operation_name: Some(tel::OPERATION_EXECUTE_TOOL.to_owned()),
                            status: if tool_result.starts_with("error:")
                                || tool_result.starts_with("permission denied")
                            {
                                Some("error".to_owned())
                            } else {
                                Some("ok".to_owned())
                            },
                            ..smedja_ingot::AuditEvent::default()
                        };
                        if let Err(e) = ingot.record_timeline_event(tool_audit).await {
                            warn!(turn_id = %turn_id, error = %e, "failed to record tool audit event");
                        }
                    }

                    // 5e. Compress the tool result (SmartCrusher strips JSON nulls,
                    // bypassed by SMEDJA_NO_TOOL_COMPRESS=1), then append it as a user
                    // message and continue the loop. Compression runs before the push
                    // so token budgeting reflects the crushed size.
                    let crushed = smedja_adapter::crush::compress_tool_result(&tool_result);
                    let escaped_result = crushed.replace('<', "&lt;").replace('>', "&gt;");
                    mem.push(AdapterMessage::user(format!(
                        "<tool_result tool=\"{tool_name}\">{escaped_result}</tool_result>"
                    )));

                    full_response = response_text;
                    continue 'tool_loop;
                }

                // 5f. No tool call — this is the final response.
                full_response = response_text;
                break 'tool_loop;
            }

            // Attempt finished. If no rotation was requested the turn succeeded
            // (or exhausted the tool-iteration cap) against this provider.
            let Some((kind, message)) = rotate else {
                break 'ring;
            };
            last_kind = kind;

            // Record the rotation on the turn span and emit a structured log line
            // naming the from/to runner and the classified kind.
            let to_runner = ring
                .get(rotations as usize + 1)
                .map_or("<none>", |e| e.runner_name);
            tel::set_span_error(&mut turn_span, kind, &message, true);
            turn_span.set_attribute(KeyValue::new(tel::ERROR_COUNT, i64::from(rotations + 1)));
            warn!(
                turn_id = %turn_id,
                from_runner = %runner,
                to_runner,
                kind,
                rotation = rotations + 1,
                "rotating provider on retryable failure"
            );

            rotations += 1;
            if rotations > MAX_PROVIDER_ROTATIONS {
                // Rotation budget spent: stop and fail with the last kind below.
                break 'ring;
            }
            // Otherwise the loop advances to the next eligible ring entry,
            // preserving the accumulated `WorkingMemory`.
        }

        // If every attempt rotated (the ring was exhausted or the rotation cap
        // was hit) the turn never produced a response: fail with the last kind.
        if full_response.is_empty() && rotations > 0 {
            let reason = format!("all eligible providers failed; last error: {last_kind}");
            warn!(turn_id = %turn_id, last_kind, "provider ring exhausted");
            let _ = ingot.update_task_status(&turn_id, "failed").await;
            dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
            tel::set_span_error(&mut turn_span, last_kind, &reason, false);
            turn_span.end();
            return;
        }

        // 6. Persist response and mark complete.
        if let Err(e) = ingot.set_task_response(&turn_id, &full_response).await {
            let reason = e.to_string();
            warn!(turn_id = %turn_id, error = %reason, "failed to store task response");
            dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
            turn_span.set_status(SpanStatus::error(reason));
            turn_span.end();
            return;
        }

        let turn_n: i64 = {
            ingot
                .list_checkpoints(&session_id)
                .await
                .map_or(0, |v| i64::try_from(v.len()).unwrap_or(i64::MAX))
        };

        // 6b. Auto-generate a title for the session on the very first completed
        // turn, if no title was set at creation time. Uses the user message
        // (task.title) without the injected graph block, truncated to 10 words.
        if turn_n == 0 && session.as_ref().is_none_or(|s| s.title.is_empty()) {
            let auto_title = derive_title(&task.title);
            if !auto_title.is_empty() {
                if let Err(e) = ingot.update_session_title(&session_id, &auto_title).await {
                    tracing::debug!(error = %e, "failed to auto-set session title; continuing");
                }
            }
        }

        // 7. Record cost entry.
        {
            let cost_usd =
                price_table.compute_cost(&model, total_input_tokens, total_output_tokens);
            let entry = CostEntry {
                id: Uuid::new_v4(),
                session_id: session_id.clone(),
                turn_n,
                runner: runner.clone(),
                model,
                input_tok: i64::from(total_input_tokens),
                output_tok: i64::from(total_output_tokens),
                cost_usd: Microdollars::from_usd_f64(cost_usd),
                created_at: Timestamp::now(),
            };
            if let Err(e) = ingot.insert_cost(entry).await {
                warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to record cost entry");
            }
        }

        // 7b. Record savings, source-tagged, on the tokens-saved ledger (never
        // the billed cost_ledger). Cache reads are provider-reported "input not
        // re-paid" (source=cache); cold-context omission is the dropped-token
        // estimate (source=cold-context). Both are advisory: a ledger error is
        // logged and swallowed and must never break the turn. Zero-valued
        // savings write no row.
        {
            if total_cache_read_tokens > 0 {
                let entry = TokensSavedEntry {
                    id: Uuid::new_v4(),
                    session_id: session_id.clone(),
                    turn_n,
                    command: "cache_read".to_owned(),
                    tokens_saved: i64::from(total_cache_read_tokens),
                    source: "cache".to_owned(),
                    created_at: Timestamp::now(),
                };
                if let Err(e) = ingot.insert_tokens_saved(entry).await {
                    warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to record cache savings");
                }
            }
            if total_cold_omitted_tokens > 0 {
                let entry = TokensSavedEntry {
                    id: Uuid::new_v4(),
                    session_id: session_id.clone(),
                    turn_n,
                    command: "cold_context".to_owned(),
                    tokens_saved: i64::try_from(total_cold_omitted_tokens).unwrap_or(i64::MAX),
                    source: "cold-context".to_owned(),
                    created_at: Timestamp::now(),
                };
                if let Err(e) = ingot.insert_tokens_saved(entry).await {
                    warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to record cold-context savings");
                }
            }
        }

        // 8. Save checkpoint.
        {
            let messages_json_value: Vec<serde_json::Value> = mem
                .messages()
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "role": match m.role {
                            AdapterRole::User => "user",
                            AdapterRole::Assistant => "assistant",
                            AdapterRole::System => "system",
                            AdapterRole::Tool => "tool",
                        },
                        "content": m.content,
                    })
                })
                .collect();
            let cp = Checkpoint {
                id: Uuid::new_v4(),
                session_id: session_id.clone(),
                turn_n,
                messages_json: serde_json::to_string(&messages_json_value)
                    .unwrap_or_else(|_| "[]".to_owned()),
                created_at: Timestamp::now(),
                compaction_id: None,
            };
            if let Err(e) = ingot.save_checkpoint(cp).await {
                warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to save checkpoint");
            }
        }

        // 9. Save per-turn token snapshot.
        {
            let input_tok = i64::from(total_input_tokens);
            let output_tok = i64::from(total_output_tokens);
            let (prior_in, prior_out) =
                ingot
                    .session_token_snapshots(&session_id)
                    .await
                    .map_or((0, 0), |snaps| {
                        snaps
                            .last()
                            .map_or((0i64, 0i64), |s| (s.cumulative_input, s.cumulative_output))
                    });
            let snap = TokenSnapshot {
                id: Uuid::new_v4(),
                session_id: session_id.clone(),
                turn_n,
                input_tok,
                output_tok,
                cumulative_input: prior_in + input_tok,
                cumulative_output: prior_out + output_tok,
                created_at: Timestamp::now(),
            };
            if let Err(e) = ingot.save_token_snapshot(snap).await {
                warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to save token snapshot");
            }
        }

        // 9b. Auto-summarise when context pressure exceeds 80%.
        if context_pressure_exceeds_threshold(total_input_tokens, turn_context_window) {
            let history: Vec<(String, String)> = mem
                .messages()
                .iter()
                .map(|m| {
                    let role = match m.role {
                        AdapterRole::User => "user",
                        AdapterRole::Assistant => "assistant",
                        _ => "system",
                    };
                    (role.to_owned(), m.content.clone())
                })
                .collect();
            let prompt = build_summariser_prompt(&history);
            let pool_entry = self
                .pool
                .get(smedja_assayer::Runner::Claude, smedja_assayer::Tier::Fast)
                .or_else(|| {
                    self.pool
                        .get(smedja_assayer::Runner::Codex, smedja_assayer::Tier::Fast)
                })
                .or_else(|| self.pool.get_default());
            if let Some(entry) = pool_entry {
                let sum_opts = smedja_adapter::CallOptions {
                    model: std::env::var("SMEDJA_SUMMARISER_MODEL")
                        .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_owned()),
                    max_tokens: Some(512),
                    temperature: Some(0.3),
                    system: Some("You are a summarisation assistant.".to_owned()),
                    tools: None,
                    provider_session_id: None,
                    smedja_session_id: None,
                    permission_mode: None,
                    stable_prefix_len: None,
                    cache_strategy: smedja_adapter::CacheStrategy::None,
                    workspace: None,
                };
                let stream = entry
                    .provider
                    .stream_chat(&[AdapterMessage::user(prompt)], &sum_opts);
                let sum_dispatcher = smedja_bellows::Dispatcher::new(1);
                match crate::common::drain_stream(
                    stream,
                    &sum_dispatcher,
                    None,
                    &CorrelationCtx::default(),
                )
                .await
                {
                    Ok((summary, _, _, _, _)) if !summary.is_empty() => {
                        let embedding = self.embedder.embed_query(&summary).await;
                        let model_id = self.embedder.model_id().to_owned();
                        let dim = self.embedder.dim();
                        let compact_sid = session_id.clone();
                        let vault = Arc::clone(&self.vault);
                        tokio::task::spawn_blocking(move || {
                            use smedja_vault::VaultEntry;
                            let entry = VaultEntry {
                                id: format!("compact:{compact_sid}:{turn_n}"),
                                embedding,
                                payload: serde_json::json!({
                                    "session_id": compact_sid,
                                    "turn_n": turn_n,
                                }),
                                namespace: "compact".to_owned(),
                                content: summary,
                                source_file: None,
                                added_by: Some("auto-summarise".to_owned()),
                                chunk_index: None,
                                parent_id: None,
                                created_at: 0.0,
                                embedder_model_id: model_id,
                                dim,
                            };
                            let mut guard = vault.blocking_lock();
                            if let Err(e) = guard.upsert(&entry) {
                                tracing::warn!(error = %e, "auto-summarise: vault upsert failed");
                            }
                        });
                        tracing::debug!(session_id = %session_id, turn_n, "auto-summarise: context compacted");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "auto-summarise: summariser call failed; continuing");
                    }
                }
            }
        }

        turn_span.set_attribute(KeyValue::new(
            tel::INPUT_TOKENS,
            i64::from(total_input_tokens),
        ));
        turn_span.set_attribute(KeyValue::new(
            tel::OUTPUT_TOKENS,
            i64::from(total_output_tokens),
        ));
        turn_span.set_attribute(KeyValue::new(tel::TIER, runner.clone()));
        turn_span.set_attribute(KeyValue::new(
            "smedja.agent.kind",
            session
                .as_ref()
                .and_then(|s| s.mode.as_deref())
                .unwrap_or("impl")
                .to_owned(),
        ));

        let sc = turn_span.span_context().clone();
        let span_trace_id = format!("{}", sc.trace_id());
        let span_span_id = format!("{}", sc.span_id());
        let traceparent = format!("00-{span_trace_id}-{span_span_id}-01");

        turn_span.end();

        // 10. Record audit event for this turn.
        {
            let agent_name_val = session
                .as_ref()
                .and_then(|s| s.mode.as_deref())
                .unwrap_or("interactive")
                .to_owned();
            let audit_ev = smedja_ingot::AuditEvent {
                id: Uuid::new_v4(),
                ts: Timestamp::now(),
                session_id: session_id.clone(),
                turn_id: Some(turn_id.clone()),
                action_type: "turn_end".into(),
                actor: "smdjad".into(),
                input_tok: i64::from(total_input_tokens),
                output_tok: i64::from(total_output_tokens),
                traceparent: Some(traceparent.clone()),
                trace_id: Some(span_trace_id),
                span_id: Some(span_span_id),
                conversation_id: Some(session_id.clone()),
                agent_name: Some(agent_name_val),
                operation_name: Some(tel::OPERATION_INVOKE_AGENT.to_owned()),
                status: Some("ok".to_owned()),
                change_name: self.active_change.clone(),
                ..smedja_ingot::AuditEvent::default()
            };
            if let Err(e) = ingot.record_timeline_event(audit_ev).await {
                warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to record turn audit event");
            }
        }

        dispatcher.publish(TurnEvent::Completed {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            output_tokens: total_output_tokens,
            input_tokens: Some(total_input_tokens),
            traceparent: Some(traceparent),
            correlation: CorrelationCtx {
                conversation_id: Some(session_id.clone()),
                trace_id: None,
                span_id: None,
                parent_span_id: None,
                operation_name: Some(tel::OPERATION_INVOKE_AGENT.to_owned()),
                agent_name: session
                    .as_ref()
                    .and_then(|s| s.mode.as_deref())
                    .map(str::to_owned),
                status: Some("ok".to_owned()),
            },
        });
    }
}

/// Formats vault recall results as a `<recalled_context>` XML block for injection
/// into the user turn. Returns `None` when `entries` is empty.
fn format_vault_recalled(entries: &[VaultEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let body = entries
        .iter()
        .map(|e| e.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    Some(format!("<recalled_context>\n{body}\n</recalled_context>"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use smedja_adapter::types::{Delta, Message as AdapterMessage};
    use smedja_adapter::{AdapterError, CallOptions, DeltaStream, Provider};
    use smedja_assayer::{Assayer, Runner, Tier};
    use smedja_bellows::{Dispatcher, TurnEvent};
    use smedja_ingot::{Ingot, IngotHandle, Session, Task};
    use smedja_types::Timestamp;
    use smedja_vault::Vault;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use crate::price_table::PriceTable;
    use crate::provider_pool::{build_provider_pool, ProviderEntry, ProviderPool};

    use smedja_methodology::MethodologyConfig;

    #[test]
    fn directive_present_under_default_config() {
        // On a code-writing turn with default config the sealed system prefix
        // carries the TDD/clean discipline directive (both clauses present).
        let directive = super::methodology_directive_for(MethodologyConfig::default(), true)
            .expect("default config must yield a directive");
        assert!(directive.contains("<methodology_discipline>"));
        assert!(directive.contains("failing test"));
        assert!(directive.contains("`unwrap`"));
    }

    #[test]
    fn tdd_clause_omitted_when_tdd_disabled() {
        let cfg = MethodologyConfig {
            tdd: false,
            clean: true,
        };
        let directive = super::methodology_directive_for(cfg, true)
            .expect("clean clause must still be present");
        assert!(!directive.contains("failing test"));
        assert!(directive.contains("`unwrap`"));
    }

    #[test]
    fn clean_clause_omitted_when_clean_disabled() {
        let cfg = MethodologyConfig {
            tdd: true,
            clean: false,
        };
        let directive =
            super::methodology_directive_for(cfg, true).expect("tdd clause must still be present");
        assert!(directive.contains("failing test"));
        assert!(!directive.contains("`unwrap`"));
    }

    #[test]
    fn directive_omitted_entirely_when_both_disabled() {
        let cfg = MethodologyConfig {
            tdd: false,
            clean: false,
        };
        assert!(super::methodology_directive_for(cfg, true).is_none());
    }

    /// A provider that yields a single classified error then nothing — used to
    /// trigger a rotation in the orchestrator.
    struct ErrorProvider {
        kind: &'static str,
    }
    impl Provider for ErrorProvider {
        fn stream_chat(&self, _messages: &[AdapterMessage], _opts: &CallOptions) -> DeltaStream {
            let err = match self.kind {
                "context_length_exceeded" => {
                    AdapterError::ContextLengthExceeded("prompt is too long".to_owned())
                }
                _ => AdapterError::QuotaExhausted("insufficient_quota".to_owned()),
            };
            Box::pin(futures_util::stream::iter(vec![Err(err)]))
        }
    }

    /// A provider that streams a fixed, tool-call-free text response plus usage.
    struct SuccessProvider {
        text: &'static str,
    }
    impl Provider for SuccessProvider {
        fn stream_chat(&self, _messages: &[AdapterMessage], _opts: &CallOptions) -> DeltaStream {
            let text = self.text.to_owned();
            Box::pin(futures_util::stream::iter(vec![
                Ok(Delta::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                }),
                Ok(Delta::Text(text)),
            ]))
        }
    }

    /// A provider that reports a fixed cache-read count alongside usage.
    struct CacheReadProvider {
        cache_read_tokens: u32,
    }
    impl Provider for CacheReadProvider {
        fn stream_chat(&self, _messages: &[AdapterMessage], _opts: &CallOptions) -> DeltaStream {
            Box::pin(futures_util::stream::iter(vec![
                Ok(Delta::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: self.cache_read_tokens,
                }),
                Ok(Delta::Text("done".to_owned())),
            ]))
        }
    }

    fn entry(
        key: (Runner, Tier),
        runner_name: &'static str,
        provider: Box<dyn Provider>,
    ) -> ((Runner, Tier), ProviderEntry) {
        (
            key,
            ProviderEntry {
                provider,
                runner: key.0,
                tier: key.1,
                runner_name,
                default_model: "test-model".to_owned(),
            },
        )
    }

    /// Seeds an in-memory ingot with a session (no mode → Orchestrator route to
    /// Claude/Fast) and a task, returning the handle and the turn id.
    async fn seed_session_and_task(prompt: &str) -> (IngotHandle, String, String) {
        let ingot = IngotHandle::new(Ingot::open_in_memory().expect("in-memory Ingot must open"));
        let session_id = Uuid::new_v4().to_string();
        let task_id = Uuid::new_v4();
        let now = Timestamp::now();
        ingot
            .create_session(Session {
                id: Uuid::parse_str(&session_id).unwrap(),
                created_at: now,
                updated_at: now,
                status: "active".to_owned(),
                task_id: None,
                mode: None,
                title: "test".to_owned(),
                cowork_mode: false,
                workspace_root: None,
                model_override: None,
                runner_override: None,
            })
            .await
            .expect("session insert");
        ingot
            .create_task(Task {
                id: task_id,
                title: prompt.to_owned(),
                description: String::new(),
                status: "planned".to_owned(),
                created_at: now,
                session_id: Some(session_id.clone()),
                response: None,
            })
            .await
            .expect("task insert");
        (ingot, session_id, task_id.to_string())
    }

    fn orchestrator_with_pool(
        ingot: IngotHandle,
        dispatcher: Arc<Dispatcher>,
        pool: ProviderPool,
    ) -> super::TurnOrchestrator {
        super::TurnOrchestrator::new(
            ingot,
            dispatcher,
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(pool),
            Arc::new(Assayer::default_rules()),
            Arc::new(PriceTable::embedded()),
            Arc::new(Mutex::new(
                Vault::open_in_memory().expect("in-memory Vault must open"),
            )),
            Arc::new(crate::embedder_port::FnvEmbedder::new()),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            None,
            Arc::new(smedja_lsp::LspManager::new()),
        )
    }

    #[tokio::test]
    async fn rotates_to_next_provider_on_quota_error_preserving_prompt() {
        let (ingot, session_id, turn_id) = seed_session_and_task("solve the problem").await;
        let dispatcher = Arc::new(Dispatcher::new(64));
        let mut rx = dispatcher.subscribe();

        // Routed entry (Claude/Fast) errors; the more-capable Claude/Deep entry
        // succeeds. The ring is [Fast, Deep].
        let pool = ProviderPool::from_entries_for_test(vec![
            entry(
                (Runner::Claude, Tier::Fast),
                "claude-cli",
                Box::new(ErrorProvider {
                    kind: "quota_exhausted",
                }),
            ),
            entry(
                (Runner::Claude, Tier::Deep),
                "claude-deep",
                Box::new(SuccessProvider {
                    text: "answer from second provider",
                }),
            ),
        ]);

        let orc = orchestrator_with_pool(ingot, Arc::clone(&dispatcher), pool);
        orc.run(session_id, turn_id).await;

        let mut completed = false;
        let mut got_second_provider_text = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                TurnEvent::Completed { .. } => completed = true,
                TurnEvent::AssistantDelta { content, .. } => {
                    if content.contains("answer from second provider") {
                        got_second_provider_text = true;
                    }
                }
                TurnEvent::Failed { reason, .. } => {
                    panic!("turn must not fail on rotation: {reason}")
                }
                _ => {}
            }
        }
        assert!(
            completed,
            "turn must complete after rotating to the second provider"
        );
        assert!(
            got_second_provider_text,
            "the completed turn must carry the second provider's response"
        );
    }

    #[tokio::test]
    async fn turn_fails_after_ring_exhausted_with_last_kind() {
        let (ingot, session_id, turn_id) = seed_session_and_task("solve the problem").await;
        let dispatcher = Arc::new(Dispatcher::new(64));
        let mut rx = dispatcher.subscribe();

        // Every ring entry yields a retryable quota error.
        let pool = ProviderPool::from_entries_for_test(vec![
            entry(
                (Runner::Claude, Tier::Fast),
                "claude-cli",
                Box::new(ErrorProvider {
                    kind: "quota_exhausted",
                }),
            ),
            entry(
                (Runner::Claude, Tier::Deep),
                "claude-deep",
                Box::new(ErrorProvider {
                    kind: "quota_exhausted",
                }),
            ),
        ]);

        let orc = orchestrator_with_pool(ingot, Arc::clone(&dispatcher), pool);
        orc.run(session_id, turn_id).await;

        let mut failure_reason = None;
        while let Ok(ev) = rx.try_recv() {
            if let TurnEvent::Failed { reason, .. } = ev {
                failure_reason = Some(reason);
            }
        }
        let reason = failure_reason.expect("turn must fail when every ring entry errors");
        assert!(
            reason.contains("quota_exhausted"),
            "failure reason must carry the last classified kind, got: {reason}"
        );
    }

    #[tokio::test]
    async fn cache_read_tokens_recorded_as_source_cache() {
        let (ingot, session_id, turn_id) = seed_session_and_task("do the thing").await;
        let dispatcher = Arc::new(Dispatcher::new(64));

        // The routed entry (Claude/Fast) reports 1234 cache-read tokens.
        let pool = ProviderPool::from_entries_for_test(vec![entry(
            (Runner::Claude, Tier::Fast),
            "claude-cli",
            Box::new(CacheReadProvider {
                cache_read_tokens: 1234,
            }),
        )]);

        let orc = orchestrator_with_pool(ingot.clone(), Arc::clone(&dispatcher), pool);
        orc.run(session_id.clone(), turn_id).await;

        let by_source = ingot
            .session_tokens_saved_by_source(&session_id)
            .await
            .unwrap();
        let cache_total: i64 = by_source
            .iter()
            .filter(|(src, _)| src == "cache")
            .map(|(_, n)| *n)
            .sum();
        assert_eq!(
            cache_total, 1234,
            "a turn reporting cache_read_input_tokens=N must write source=cache, tokens_saved=N"
        );
    }

    #[tokio::test]
    async fn zero_cache_reads_write_no_cache_row() {
        let (ingot, session_id, turn_id) = seed_session_and_task("do the thing").await;
        let dispatcher = Arc::new(Dispatcher::new(64));

        let pool = ProviderPool::from_entries_for_test(vec![entry(
            (Runner::Claude, Tier::Fast),
            "claude-cli",
            Box::new(CacheReadProvider {
                cache_read_tokens: 0,
            }),
        )]);

        let orc = orchestrator_with_pool(ingot.clone(), Arc::clone(&dispatcher), pool);
        orc.run(session_id.clone(), turn_id).await;

        let by_source = ingot
            .session_tokens_saved_by_source(&session_id)
            .await
            .unwrap();
        assert!(
            !by_source.iter().any(|(src, _)| src == "cache"),
            "a zero cache-read turn must write no source=cache row"
        );
    }

    #[tokio::test]
    async fn rotation_records_error_kind_and_retryable() {
        use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
        use opentelemetry_sdk::trace::TracerProvider;

        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        opentelemetry::global::set_tracer_provider(provider.clone());

        let (ingot, session_id, turn_id) = seed_session_and_task("solve the problem").await;
        let dispatcher = Arc::new(Dispatcher::new(64));

        let pool = ProviderPool::from_entries_for_test(vec![
            entry(
                (Runner::Claude, Tier::Fast),
                "claude-cli",
                Box::new(ErrorProvider {
                    kind: "quota_exhausted",
                }),
            ),
            entry(
                (Runner::Claude, Tier::Deep),
                "claude-deep",
                Box::new(SuccessProvider {
                    text: "answer from second provider",
                }),
            ),
        ]);

        let orc = orchestrator_with_pool(ingot, Arc::clone(&dispatcher), pool);
        orc.run(session_id.clone(), turn_id.clone()).await;

        let _ = provider.force_flush();
        let spans = exporter.get_finished_spans().expect("spans");
        // Locate this turn's agent-invoke span by its unique turn id attribute.
        let turn_span = spans
            .iter()
            .filter(|s| s.name == super::tel::SPAN_AGENT_INVOKE)
            .find(|s| {
                s.attributes.iter().any(|kv| {
                    kv.key.as_str() == "smedja.turn.id" && kv.value.as_str() == turn_id.as_str()
                })
            })
            .expect("this turn's agent-invoke span must be exported");

        let kind = turn_span
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "smedja.error.kind")
            .map(|kv| kv.value.as_str().to_string());
        assert_eq!(
            kind.as_deref(),
            Some("quota_exhausted"),
            "rotation must record smedja.error.kind on the turn span"
        );
        assert!(
            turn_span
                .attributes
                .iter()
                .any(|kv| kv.key.as_str() == "smedja.error.retryable"),
            "rotation must record smedja.error.retryable on the turn span"
        );
    }

    #[test]
    fn fast_tier_prompt_no_larger_than_deep_with_hot_present() {
        use smedja_adapter::types::Message;
        use smedja_assayer::Tier;
        use smedja_memory::WorkingMemory;

        let build = |tier: Tier| {
            let (strata, budget) = super::strata_for_tier(tier);
            let mut m = WorkingMemory::new(budget);
            m.set_strata(strata);
            m.push(Message::user("stable context")); // prefix
            m.seal_prefix();
            for i in 0..40 {
                m.push(Message::user(format!(
                    "turn {i} with enough content to cost a few tokens each"
                )));
            }
            m.build_prompt(budget)
        };

        let fast = build(Tier::Fast);
        let deep = build(Tier::Deep);

        // A shallower/cheaper tier must never assemble more messages than deep.
        assert!(
            fast.len() <= deep.len(),
            "fast prompt ({}) must be ≤ deep prompt ({})",
            fast.len(),
            deep.len()
        );
        // The most recent hot turn must be present in both regardless of tier.
        assert!(
            fast.iter().any(|m| m.content.contains("turn 39")),
            "fast must retain the latest hot turn"
        );
        assert!(
            deep.iter().any(|m| m.content.contains("turn 39")),
            "deep must retain the latest hot turn"
        );
    }

    #[test]
    fn model_context_window_known_and_default() {
        assert_eq!(super::model_context_window("claude-sonnet-4-6"), 200_000);
        assert_eq!(super::model_context_window("some-unknown-model"), 128_000);
    }

    #[tokio::test]
    async fn orchestrator_returns_error_for_unknown_session() {
        use smedja_bellows::TurnEvent;

        // Arrange: build the shared dispatcher first so we can subscribe before run().
        let ingot = IngotHandle::new(Ingot::open_in_memory().expect("in-memory Ingot must open"));
        let dispatcher = Arc::new(Dispatcher::new(64));
        let mut rx = dispatcher.subscribe();
        let gates = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let pool = Arc::new(build_provider_pool().await);
        let assayer = Arc::new(Assayer::default_rules());
        let price_table = Arc::new(PriceTable::embedded());
        let vault = Arc::new(Mutex::new(
            Vault::open_in_memory().expect("in-memory Vault must open"),
        ));

        let provider_sessions = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let cache_aligners = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let orc = super::TurnOrchestrator::new(
            ingot,
            Arc::clone(&dispatcher),
            gates,
            pool,
            assayer,
            price_table,
            vault,
            Arc::new(crate::embedder_port::FnvEmbedder::new()),
            provider_sessions,
            cache_aligners,
            None,
            Arc::new(smedja_lsp::LspManager::new()),
        );

        let session_id = "sess-does-not-exist".to_owned();
        let turn_id = "turn-does-not-exist".to_owned();

        // Act: run the orchestrator with an unknown turn_id.
        orc.run(session_id.clone(), turn_id.clone()).await;

        // Assert: a Fail event must have been published.
        let mut got_fail = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, TurnEvent::Failed { .. }) {
                got_fail = true;
                break;
            }
        }
        assert!(
            got_fail,
            "orchestrator must publish TurnEvent::Failed for an unknown task"
        );
    }

    /// Cross-turn persistence of the per-`(session, runner)` aligner.
    ///
    /// These tests model exactly what the ring loop does: look up (or default)
    /// the aligner for a key in the shared [`super::CacheAligners`] map, call
    /// `align` against the freshly-sealed memory, and store the mutated aligner
    /// back. A persisted aligner observes the prior turn and reports real
    /// `Grown`/`Mutated` drift; distinct runner keys never share history.
    mod cache_aligner_persistence {
        use std::sync::Arc;

        use smedja_adapter::types::Message as AdapterMessage;
        use smedja_memory::{Drift, WorkingMemory};
        use tokio::sync::Mutex;

        use crate::orchestrator::{AlignerKey, CacheAligners};

        /// Builds a sealed [`WorkingMemory`] whose stable prefix is `prefix`.
        fn sealed(prefix: &[&str]) -> WorkingMemory {
            let mut mem = WorkingMemory::new(4096);
            for content in prefix {
                mem.push(AdapterMessage::system(*content));
            }
            mem.seal_prefix();
            mem
        }

        /// Mirrors the ring-loop get-or-insert: take-or-default under the lock,
        /// align, re-insert, and return the hint.
        async fn align_persisted(
            aligners: &CacheAligners,
            key: &AlignerKey,
            mem: &WorkingMemory,
        ) -> Drift {
            let mut guard = aligners.lock().await;
            let mut aligner = guard.remove(key).unwrap_or_default();
            let hint = aligner.align(mem);
            guard.insert(key.clone(), aligner);
            hint.drift
        }

        #[tokio::test]
        async fn second_turn_same_session_runner_reports_grown() {
            let aligners: CacheAligners = Arc::new(Mutex::new(std::collections::HashMap::new()));
            let key: AlignerKey = ("sess-1".to_owned(), "anthropic".to_owned());

            let first = align_persisted(&aligners, &key, &sealed(&["sys", "skills"])).await;
            assert_eq!(first, Drift::Unchanged, "first turn has no prior history");

            // Same leading messages, prefix grew by one settled turn.
            let second =
                align_persisted(&aligners, &key, &sealed(&["sys", "skills", "settled turn"])).await;
            assert_eq!(
                second,
                Drift::Grown,
                "a persisted aligner must observe the prior boundary and report Grown, not a fresh Unchanged"
            );
        }

        #[tokio::test]
        async fn distinct_runner_keys_do_not_share_history() {
            let aligners: CacheAligners = Arc::new(Mutex::new(std::collections::HashMap::new()));
            let anthropic: AlignerKey = ("sess-1".to_owned(), "anthropic".to_owned());
            let openai: AlignerKey = ("sess-1".to_owned(), "openai".to_owned());

            // Anthropic observes a grown prefix across two turns.
            let _ = align_persisted(&aligners, &anthropic, &sealed(&["sys", "skills"])).await;
            let grown = align_persisted(
                &aligners,
                &anthropic,
                &sealed(&["sys", "skills", "settled"]),
            )
            .await;
            assert_eq!(grown, Drift::Grown);

            // A failover to openai (same session) must start fresh: first turn is
            // Unchanged at the full prefix, never compared against anthropic's history.
            let openai_first =
                align_persisted(&aligners, &openai, &sealed(&["sys", "skills", "settled"])).await;
            assert_eq!(
                openai_first,
                Drift::Unchanged,
                "a fresh runner key must not inherit the prior runner's prefix digests"
            );
        }

        #[tokio::test]
        async fn mutated_message_inside_prior_boundary_reports_mutated() {
            let aligners: CacheAligners = Arc::new(Mutex::new(std::collections::HashMap::new()));
            let key: AlignerKey = ("sess-1".to_owned(), "anthropic".to_owned());

            let _ = align_persisted(&aligners, &key, &sealed(&["sys", "skills", "context"])).await;

            // Second turn: index 1 changed content inside the prior boundary.
            let second =
                align_persisted(&aligners, &key, &sealed(&["sys", "CHANGED", "context"])).await;
            assert_eq!(
                second,
                Drift::Mutated,
                "a message changing inside the prior sealed boundary must report Mutated"
            );
        }
    }

    // --- derive_title tests ---

    #[test]
    fn derive_title_takes_first_ten_words() {
        let input = "one two three four five six seven eight nine ten eleven twelve";
        let title = super::derive_title(input);
        assert_eq!(title, "one two three four five six seven eight nine ten");
    }

    #[test]
    fn derive_title_short_input_unchanged() {
        let title = super::derive_title("fix the bug");
        assert_eq!(title, "fix the bug");
    }

    #[test]
    fn derive_title_strips_graph_injection_block() {
        let input = "refactor auth module\n\n<graph_symbols>\nsome code\n</graph_symbols>";
        let title = super::derive_title(input);
        assert_eq!(title, "refactor auth module");
    }

    #[test]
    fn derive_title_empty_input_returns_empty() {
        assert_eq!(super::derive_title(""), "");
    }

    // --- format_lsp_diagnostics tests ---

    #[test]
    fn format_lsp_diagnostics_empty_snapshot_returns_none() {
        let snap = smedja_lsp::LspSnapshot::default();
        assert!(super::format_lsp_diagnostics(&snap).is_none());
    }

    #[test]
    fn format_lsp_diagnostics_errors_and_warnings_included() {
        use smedja_lsp::types::{Diagnostic, Severity};
        use std::path::PathBuf;
        let snap = smedja_lsp::LspSnapshot {
            servers: vec![],
            diagnostics: vec![
                Diagnostic {
                    file: PathBuf::from("src/main.rs"),
                    line: 42,
                    col: 1,
                    severity: Severity::Error,
                    code: Some("E0308".to_owned()),
                    message: "mismatched types".to_owned(),
                },
                Diagnostic {
                    file: PathBuf::from("src/lib.rs"),
                    line: 17,
                    col: 5,
                    severity: Severity::Warning,
                    code: None,
                    message: "unused variable".to_owned(),
                },
            ],
        };
        let block = super::format_lsp_diagnostics(&snap).unwrap();
        assert!(block.contains("<lsp_diagnostics>"));
        assert!(block.contains("src/main.rs:42"));
        assert!(block.contains("mismatched types"));
        assert!(block.contains("src/lib.rs:17"));
        assert!(block.contains("unused variable"));
    }

    #[test]
    fn format_lsp_diagnostics_caps_at_twenty_lines() {
        use smedja_lsp::types::{Diagnostic, Severity};
        use std::path::PathBuf;
        let diags: Vec<Diagnostic> = (0..30)
            .map(|i| Diagnostic {
                file: PathBuf::from("src/main.rs"),
                line: i,
                col: 1,
                severity: Severity::Error,
                code: None,
                message: format!("err {i}"),
            })
            .collect();
        let snap = smedja_lsp::LspSnapshot {
            servers: vec![],
            diagnostics: diags,
        };
        let block = super::format_lsp_diagnostics(&snap).unwrap();
        let lines: Vec<&str> = block.lines().collect();
        // header + up to 20 diag lines + footer + optional truncation line
        assert!(lines.len() <= 23, "too many lines: {}", lines.len());
    }

    // --- build_summariser_prompt tests ---

    #[test]
    fn build_summariser_prompt_includes_history() {
        let history = vec![
            ("user".to_owned(), "fix the auth bug".to_owned()),
            (
                "assistant".to_owned(),
                "I found the issue in auth.rs".to_owned(),
            ),
        ];
        let prompt = super::build_summariser_prompt(&history);
        assert!(prompt.contains("fix the auth bug"));
        assert!(prompt.contains("I found the issue in auth.rs"));
    }

    #[test]
    fn build_summariser_prompt_has_instruction() {
        let prompt = super::build_summariser_prompt(&[]);
        assert!(prompt.contains("summarise") || prompt.contains("summary"));
    }

    #[test]
    fn build_summariser_prompt_caps_turns() {
        let history: Vec<(String, String)> = (0..30)
            .map(|i| ("user".to_owned(), format!("turn {i}")))
            .collect();
        let prompt = super::build_summariser_prompt(&history);
        // Should not include all 30 turns verbatim — cap enforced
        let turn_count = prompt.matches("turn ").count();
        assert!(turn_count <= 20, "too many turns: {turn_count}");
    }

    // --- context_pressure_exceeds_threshold tests ---

    #[test]
    fn pressure_below_threshold_is_not_exceeded() {
        assert!(!super::context_pressure_exceeds_threshold(79_999, 100_000));
    }

    #[test]
    fn pressure_at_threshold_is_exceeded() {
        assert!(super::context_pressure_exceeds_threshold(80_000, 100_000));
    }

    #[test]
    fn pressure_with_zero_window_is_never_exceeded() {
        assert!(!super::context_pressure_exceeds_threshold(1_000, 0));
    }

    // --- format_vault_recalled tests ---

    fn make_vault_entry(content: &str) -> smedja_vault::VaultEntry {
        smedja_vault::VaultEntry {
            id: "test-id".into(),
            embedding: vec![0.1; 128],
            payload: serde_json::Value::Null,
            namespace: "default".into(),
            content: content.into(),
            source_file: None,
            added_by: None,
            chunk_index: None,
            parent_id: None,
            created_at: 0.0,
            embedder_model_id: "fnv-bow-128".into(),
            dim: 128,
        }
    }

    #[test]
    fn format_vault_recalled_empty_returns_none() {
        assert!(super::format_vault_recalled(&[]).is_none());
    }

    #[test]
    fn format_vault_recalled_single_entry_wraps_in_xml() {
        let entries = vec![make_vault_entry("the auth token expires after 24 hours")];
        let result = super::format_vault_recalled(&entries).unwrap();
        assert!(result.starts_with("<recalled_context>"));
        assert!(result.contains("auth token expires after 24 hours"));
        assert!(result.ends_with("</recalled_context>"));
    }

    #[test]
    fn format_vault_recalled_multiple_entries_joined_with_separator() {
        let entries = vec![make_vault_entry("note one"), make_vault_entry("note two")];
        let result = super::format_vault_recalled(&entries).unwrap();
        assert!(result.contains("note one"));
        assert!(result.contains("note two"));
        assert!(result.contains("---"));
    }
}
