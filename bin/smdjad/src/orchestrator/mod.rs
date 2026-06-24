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
use smedja_ingot::{Checkpoint, CostEntry, IngotHandle, TokenSnapshot};
use smedja_memory::{estimate_messages_tokens, estimate_tokens, inject_conciseness, WorkingMemory};
use smedja_types::{Microdollars, Timestamp};
use smedja_vault::Vault;
use tokio::sync::Mutex;
use tracing::warn;
use uuid::Uuid;

use smedja_telemetry as tel;

use crate::cowork::{ApprovalPrompt, CoworkGate, Decision};
use crate::executor::execute_tool;
use crate::price_table::PriceTable;
use crate::provider_pool::ProviderPool;

mod cold;
use cold::VaultColdStore;

mod context;
use context::{classify_tool_outcome, cold_k_for_tier, model_context_window, strata_for_tier};

/// Shared map from session-resume keys to provider-native resume identifiers.
///
/// Constructed once in `main()` and threaded explicitly to every orchestrator
/// (replacing the former process-static `OnceLock` singleton) so tests can
/// supply their own map.
pub(crate) type ProviderSessions = Arc<Mutex<HashMap<String, String>>>;

/// Owns all the shared resources needed to execute a single agent turn.
pub(crate) struct TurnOrchestrator {
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    assayer: Arc<Assayer>,
    price_table: Arc<PriceTable>,
    vault: Arc<Mutex<Vault>>,
    provider_sessions: ProviderSessions,
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
        provider_sessions: ProviderSessions,
    ) -> Self {
        Self {
            ingot,
            dispatcher,
            gates,
            pool,
            assayer,
            price_table,
            vault,
            provider_sessions,
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
        let provider_sessions = &self.provider_sessions;

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
        let route = {
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
            let complexity = Complexity::Coding;
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
            let base = format!("You are smedja, an AI coding assistant.{task_prefix}");
            match smedja_memory::load_workspace_skills(&workspace_root) {
                Ok(skills) if !skills.is_empty() => {
                    let joined = skills.join("\n\n");
                    format!("{base}\n\n<workspace_skills>\n{joined}\n</workspace_skills>")
                }
                Ok(_) => base,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to load workspace skills; continuing without");
                    base
                }
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
                "description": "Search the smedja vault for semantically similar entries.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "namespace": { "type": "string" },
                        "k": { "type": "integer" }
                    },
                    "required": ["query"]
                }
            }),
            serde_json::json!({
                "name": "smedja_vault_store",
                "description": "Store an entry in the smedja vault for future retrieval.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string" },
                        "namespace": { "type": "string" },
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
        let cold_adapter = Arc::new(VaultColdStore::new(Arc::clone(vault)));
        let mut mem = WorkingMemory::new(budget_tokens).with_cold_store(cold_adapter);
        mem.set_strata(strata);
        // Cold recall scales with tier depth: fast favours latency (k=1), deep
        // favours recall (k=5). The "compact" namespace is where session.compact
        // indexes its summaries.
        mem.set_cold_query("compact", cold_k_for_tier(route.tier));

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
                let graph_db_path = workspace_root.join(".smedja").join("graph.db");
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
            content
        };

        // 4a. Cold recall: pull semantically-relevant context from beyond the
        //     warm window and inject it as a single delimited system block ahead
        //     of the user turn, so it falls inside the sealed prefix. The block
        //     is capped at a fraction of the tier budget; lowest-scored entries
        //     are dropped until it fits, so cold context never displaces hot
        //     turns.
        let cold_results = mem.cold_context(&task.title).await;
        let cold_budget = budget_tokens / COLD_BUDGET_DIVISOR;
        let cold_injected = match cold::assemble_cold_block(&cold_results, cold_budget) {
            Some((block, count)) => {
                mem.push(block);
                count
            }
            None => 0,
        };
        tracing::debug!(
            smedja.turn.cold_results_injected = cold_injected,
            "cold context injection"
        );

        mem.push(AdapterMessage::user(first_user_content));
        mem.seal_prefix();

        // Observe the sealed prefix for cross-turn drift and select a safe cache
        // breakpoint. The hint feeds both `stable_prefix_len` (Anthropic, OpenAI)
        // and the per-runner `cache_strategy` below.
        let cache_hint = smedja_memory::CacheAligner::new().align(&mem);

        // 4b. Mark in_progress.
        {
            if let Err(e) = ingot.update_task_status(&turn_id, "in_progress").await {
                warn!(turn_id = %turn_id, error = %e, "failed to mark task in_progress");
            }
        }

        let mut full_response = String::new();
        let mut total_input_tokens = 0u32;
        let mut total_output_tokens = 0u32;

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

        // Drive the turn over the eligible ring. On a retryable failure the loop
        // advances to the next entry (bounded by MAX_PROVIDER_ROTATIONS),
        // re-deriving CallOptions for the new provider while preserving the same
        // WorkingMemory prompt and accumulated tool history.
        'ring: for entry in &ring {
            let entry_runner_name = entry.runner_name.to_owned();
            let runner_enum = entry.runner;
            let session_store_key = crate::common::runner_session_key(runner_enum);

            // Re-derive the model for this entry: explicit route/env/session
            // override take precedence over the entry's default model.
            let entry_model = route
                .model
                .clone()
                .or_else(|| std::env::var("SMEDJA_MODEL").ok())
                .unwrap_or_else(|| entry.default_model.to_owned());
            let entry_model = session
                .as_ref()
                .and_then(|s| s.model_override.clone())
                .unwrap_or(entry_model);
            let context_window = model_context_window(&entry_model);

            turn_span.set_attribute(KeyValue::new(tel::GEN_AI_SYSTEM, entry_runner_name.clone()));
            turn_span.set_attribute(KeyValue::new(tel::REQUEST_MODEL, entry_model.clone()));

            // Resolve the provider-native resume id from the NEW runner's session
            // key; a resume id from a previously-failed runner is never carried
            // across providers.
            let provider_session_id = if matches!(runner_enum, Runner::Claude | Runner::Codex) {
                provider_sessions
                    .lock()
                    .await
                    .get(session_store_key)
                    .cloned()
            } else {
                None
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
                stable_prefix_len,
                cache_strategy,
            };

            runner = entry_runner_name.clone();
            model = entry_model.clone();
            let provider = &entry.provider;

            // Classified failure that triggers a rotation to the next ring entry.
            // `None` means the attempt completed (success or fatal handled inline).
            let mut rotate: Option<(&'static str, String)> = None;

            'tool_loop: for _iteration in 0..crate::common::effective_max_tool_turns() {
                // 5a. Stream LLM response with rate-limit retry.
                let (response_text, input_tokens, output_tokens, native_session_id) = {
                    let mut backoff_secs = RATE_LIMIT_BACKOFF_BASE_SECS;
                    let mut attempt = 0u32;
                    // Assemble the budgeted prompt and apply verbosity steering for
                    // this iteration. The prompt always leads with the sealed prefix
                    // and all hot turns; warm turns are included until the budget is
                    // spent. The conciseness directive is appended above 60% fill.
                    let prompt = mem.build_prompt(budget_tokens);
                    let used = estimate_messages_tokens(&prompt)
                        + estimate_tokens(opts.system.as_deref().unwrap_or(""));
                    opts.system = Some(inject_conciseness(&base_system, used, context_window));
                    loop {
                        let stream = provider.stream_chat(&prompt, &opts);
                        let drain_result = tokio::time::timeout(
                            std::time::Duration::from_mins(5),
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
                                let reason = "stream timed out after 300s".to_owned();
                                warn!(turn_id = %turn_id, "provider stream timed out");
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
                            .insert(session_store_key.to_owned(), native_session_id);
                    }
                }
                total_input_tokens = total_input_tokens.saturating_add(input_tokens);
                total_output_tokens = total_output_tokens.saturating_add(output_tokens);

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

                    // 5c. Cowork gate intercept.
                    let cowork_denied = if session.as_ref().is_some_and(|s| s.cowork_mode) {
                        if let Some(gate) = gates.lock().await.get(&session_id).cloned() {
                            let ap = ApprovalPrompt {
                                step_n: 0,
                                tool: tool_name.clone(),
                                args_scrubbed: serde_json::from_str(&tool_input)
                                    .unwrap_or(serde_json::Value::Null),
                                reasoning: String::new(),
                                plan_summary: String::new(),
                            };
                            match gate.intercept(ap, 300).await {
                                Decision::Approve => None,
                                Decision::Deny(reason) => Some(format!("denied: {reason}")),
                                Decision::Modify(new_input) => {
                                    tool_input = new_input;
                                    None
                                }
                            }
                        } else {
                            None
                        }
                    } else {
                        None
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
                }),
                Ok(Delta::Text(text)),
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
                default_model: "test-model",
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
            Arc::new(Mutex::new(std::collections::HashMap::new())),
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
        let orc = super::TurnOrchestrator::new(
            ingot,
            Arc::clone(&dispatcher),
            gates,
            pool,
            assayer,
            price_table,
            vault,
            provider_sessions,
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
}
