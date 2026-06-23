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
use smedja_assayer::{Assayer, Complexity, Role as AgentRole, Route, Runner, Tier};
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_ingot::{Checkpoint, CostEntry, IngotHandle, TokenSnapshot};
use smedja_memory::{
    estimate_messages_tokens, estimate_tokens, inject_conciseness, StrataConfig, WorkingMemory,
};
use smedja_vault::Vault;
use tokio::sync::Mutex;
use tracing::warn;
use uuid::Uuid;

use smedja_telemetry as tel;

use crate::cowork::{ApprovalPrompt, CoworkGate, Decision};
use crate::executor::execute_tool;
use crate::price_table::PriceTable;
use crate::provider_pool::ProviderPool;

/// Maps a routed runner tier to its retention strata and warm-stratum token
/// budget. `fast` keeps a shallow warm window and small budget; `deep` keeps the
/// full warm window and a large budget; `local` sits between. The stable prefix
/// and hot turns are always included verbatim regardless of budget.
pub(crate) fn strata_for_tier(tier: Tier) -> (StrataConfig, usize) {
    match tier {
        Tier::Fast => (StrataConfig::fast(), 4_000),
        Tier::Local => (StrataConfig::local(), 8_000),
        Tier::Deep => (StrataConfig::deep(), 32_000),
    }
}

/// Returns the approximate context-window size (in tokens) for a model.
///
/// Used to scale verbosity steering — the conciseness directive is appended once
/// the assembled prompt exceeds 60% of this window. Unknown models fall back to a
/// conservative 128k window.
fn model_context_window(model: &str) -> usize {
    if model.to_lowercase().contains("claude") {
        200_000
    } else {
        // gpt-4o / o1 / o3 and unknown models share the conservative default.
        128_000
    }
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
}

impl TurnOrchestrator {
    pub(crate) fn new(
        ingot: IngotHandle,
        dispatcher: Arc<Dispatcher>,
        gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
        pool: Arc<ProviderPool>,
        assayer: Arc<Assayer>,
        price_table: Arc<PriceTable>,
        vault: Arc<Mutex<Vault>>,
    ) -> Self {
        Self {
            ingot,
            dispatcher,
            gates,
            pool,
            assayer,
            price_table,
            vault,
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

        let ingot = &self.ingot;
        let dispatcher = &self.dispatcher;
        let gates = &self.gates;
        let pool = &self.pool;
        let assayer = &self.assayer;
        let price_table = &self.price_table;
        let vault = &self.vault;

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
                .and_then(crate::parse_session_mode_to_role)
                .unwrap_or(AgentRole::Orchestrator);
            let complexity = Complexity::Coding;
            tracing::debug!(
                turn_id = %turn_id,
                role = ?role,
                complexity = ?complexity,
                "routing turn"
            );
            assayer.route(role, complexity)
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
                    .and_then(|r| crate::parse_runner_str(&r))
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

        let Some(pool_entry) = pool.get(route.runner, route.tier) else {
            let reason = "no LLM provider available; turn cannot execute".to_owned();
            warn!(session_id = %session_id, turn_id = %turn_id, "{reason}");
            let _ = ingot.update_task_status(&turn_id, "failed").await;
            dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
            turn_span.set_status(SpanStatus::error(reason));
            turn_span.end();
            return;
        };

        let provider = &pool_entry.provider;
        let runner = pool_entry.runner_name.to_owned();
        let runner_enum = route.runner;

        let model = route
            .model
            .clone()
            .or_else(|| std::env::var("SMEDJA_MODEL").ok())
            .unwrap_or_else(|| pool_entry.default_model.to_owned());

        // 3. Load session for workspace root, cowork mode, and task context.
        let session = { ingot.get_session(&session_id).await.ok().flatten() };

        let model = session
            .as_ref()
            .and_then(|s| s.model_override.clone())
            .unwrap_or(model);

        turn_span.set_attribute(KeyValue::new(tel::GEN_AI_SYSTEM, runner.clone()));
        turn_span.set_attribute(KeyValue::new(tel::REQUEST_MODEL, model.clone()));
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
                                crate::xml_escape(&active_task.title),
                                crate::xml_escape(active_task.description.as_str()),
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
                (crate::now_epoch() - h.last_checked) > 30.0
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

        let session_store_key = crate::runner_session_key(runner_enum);
        let provider_session_id = if matches!(runner_enum, Runner::Claude | Runner::Codex) {
            crate::provider_session_store()
                .lock()
                .await
                .get(session_store_key)
                .cloned()
        } else {
            None
        };

        // Per-runner-tier strata + token budget. `fast` keeps a shallow warm
        // window and small budget; `deep` keeps the full warm window and a large
        // budget; `local` sits between. The budget caps the warm stratum — the
        // stable prefix and hot turns are always included verbatim.
        let (strata, budget_tokens) = strata_for_tier(route.tier);
        let context_window = model_context_window(&model);

        // 4. Assemble the stable prefix (the user turn plus auto-injected graph
        //    symbols) into working memory, then seal it so the prefix survives
        //    the tool loop unchanged and drives the provider KV-cache hint.
        let mut mem = WorkingMemory::new(budget_tokens);
        mem.set_strata(strata);

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

        mem.push(AdapterMessage::user(first_user_content));
        mem.seal_prefix();

        let mut opts = CallOptions {
            model: model.clone(),
            max_tokens: Some(2048),
            temperature: Some(0.7),
            system: Some(base_system.clone()),
            tools: if all_tools.is_empty() {
                None
            } else {
                Some(all_tools)
            },
            provider_session_id,
            // F-21: cache through the real sealed prefix, not just the system
            // block. `None` for providers without prompt caching.
            stable_prefix_len: if runner == "anthropic" {
                Some(mem.stable_prefix())
            } else {
                None
            },
        };

        // 4b. Mark in_progress.
        {
            if let Err(e) = ingot.update_task_status(&turn_id, "in_progress").await {
                warn!(turn_id = %turn_id, error = %e, "failed to mark task in_progress");
            }
        }

        let mut full_response = String::new();
        let mut total_input_tokens = 0u32;
        let mut total_output_tokens = 0u32;

        'tool_loop: for _iteration in 0..crate::effective_max_tool_turns() {
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
                        crate::drain_stream(stream, dispatcher, Some(turn_id.as_str())),
                    )
                    .await;
                    match drain_result {
                        Ok(Ok(triple)) => break triple,
                        Ok(Err(crate::DrainError::RateLimited { retry_after })) => {
                            attempt += 1;
                            if attempt > MAX_RATE_LIMIT_RETRIES {
                                let reason =
                                    "rate limited by provider; retry limit exceeded".to_owned();
                                warn!(turn_id = %turn_id, "rate limit retry limit exceeded");
                                let _ = ingot.update_task_status(&turn_id, "failed").await;
                                dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
                                turn_span.set_status(SpanStatus::error(reason));
                                turn_span.end();
                                return;
                            }
                            let sleep_secs =
                                retry_after.map_or(backoff_secs, |d| d.as_secs().max(1));
                            warn!(
                                turn_id = %turn_id,
                                attempt,
                                sleep_secs,
                                "rate limited by provider; backing off"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
                            backoff_secs = (backoff_secs * 2).min(60);
                        }
                        Ok(Err(crate::DrainError::Other(reason))) => {
                            warn!(turn_id = %turn_id, error = %reason, "stream error during turn");
                            let _ = ingot.update_task_status(&turn_id, "failed").await;
                            dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
                            turn_span.set_status(SpanStatus::error(reason));
                            turn_span.end();
                            return;
                        }
                        Err(_elapsed) => {
                            let reason = "stream timed out after 300s".to_owned();
                            warn!(turn_id = %turn_id, "provider stream timed out");
                            let _ = ingot.update_task_status(&turn_id, "failed").await;
                            dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
                            turn_span.set_status(SpanStatus::error(reason));
                            turn_span.end();
                            return;
                        }
                    }
                }
            };
            if matches!(runner_enum, Runner::Claude | Runner::Codex) {
                if let Some(native_session_id) = native_session_id {
                    crate::provider_session_store()
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
                    tool_span.set_attribute(KeyValue::new(tel::TOOL_CALL_ID, tool_call_id.clone()));
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
                    if result.starts_with("error:") || result.starts_with("permission denied") {
                        use opentelemetry::trace::Span as _;
                        tool_span.set_status(opentelemetry::trace::Status::error(
                            result.chars().take(120).collect::<String>(),
                        ));
                    } else {
                        use opentelemetry::trace::Span as _;
                        tool_span.set_status(opentelemetry::trace::Status::Ok);
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
                        ts: crate::now_epoch(),
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
                cost_usd,
                created_at: crate::now_epoch(),
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
                created_at: crate::now_epoch(),
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
                created_at: crate::now_epoch(),
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
                ts: crate::now_epoch(),
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
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use smedja_assayer::Assayer;
    use smedja_bellows::Dispatcher;
    use smedja_ingot::{Ingot, IngotHandle};
    use smedja_vault::Vault;
    use tokio::sync::Mutex;

    use crate::price_table::PriceTable;
    use crate::provider_pool::build_provider_pool;

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

        let orc = super::TurnOrchestrator::new(
            ingot,
            Arc::clone(&dispatcher),
            gates,
            pool,
            assayer,
            price_table,
            vault,
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
