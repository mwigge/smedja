//! Per-turn execution state and phase methods for [`TurnOrchestrator::run`].
//!
//! A single agent turn threads roughly thirty mutable locals through a labeled
//! provider-rotation ring loop and an inner tool loop. [`TurnRun`] hoists that
//! state into one struct so [`super::TurnOrchestrator::run`] can drive the turn
//! as a short sequence of phase methods —
//! [`TurnRun::route`] → [`TurnRun::build_context`] → [`TurnRun::attempt_entry`]
//! (per ring entry) → [`TurnRun::finalize`] — instead of one god-method.
//!
//! The labeled-break control flow (`break 'ring` / `break 'tool_loop` / early
//! `return`) that used to span those blocks is expressed by the [`Flow`] signal
//! returned from [`TurnRun::attempt_entry`]: the driver in `run` matches on it.
//! The inner `'tool_loop` stays a plain labeled loop *inside* `attempt_entry`,
//! so only the ring-crossing signals became return values.

use std::fmt::Write as _;
use std::sync::Arc;

use opentelemetry::{
    global,
    trace::{Span as _, Status as SpanStatus, Tracer as _},
    KeyValue,
};
use smedja_adapter::types::{Message as AdapterMessage, Role as AdapterRole};
use smedja_adapter::CallOptions;
use smedja_assayer::{AgentRole, Complexity, Route, Runner};
use smedja_bellows::event::CorrelationCtx;
use smedja_bellows::TurnEvent;
use smedja_ingot::{Checkpoint, CostEntry, Session, Task, TokenSnapshot, TokensSavedEntry};
use smedja_memory::{estimate_messages_tokens, estimate_tokens, inject_conciseness, WorkingMemory};
use smedja_types::{Microdollars, Timestamp};
use tracing::warn;
use uuid::Uuid;

use smedja_telemetry as tel;

use crate::cowork::{CoworkGate, Decision};
use crate::executor::execute_tool;
use crate::provider_pool::ProviderEntry;

use super::cold::{self, VaultColdStore};
use super::context::{
    cache_options_for_runner, classify_tool_outcome, cold_k_for_tier, model_context_window,
    strata_for_tier,
};
use super::prompt::{
    self, build_summariser_prompt, build_turn_context, derive_title, format_lsp_diagnostics,
    format_vault_recalled, sanitize_unicode_tags,
};
use super::routing::{
    compact_threshold_from_env, context_pressure_exceeds_threshold, AlignerKey,
    ProviderSessionEntry,
};
use super::tools_catalog;
use super::{
    append_edit_diagnostics, tool_diff_content, tool_status_from_result, TurnOrchestrator,
};

/// A turn rotates to at most this many alternative providers before failing.
const MAX_PROVIDER_ROTATIONS: u32 = 3;
/// Provider back-off budget for a single rate-limited call before rotating.
const MAX_RATE_LIMIT_RETRIES: u32 = 4;
/// Base back-off (seconds) for the rate-limit retry loop.
const RATE_LIMIT_BACKOFF_BASE_SECS: u64 = 1;
/// Cold context is bounded to at most this fraction (1/N) of the tier budget.
const COLD_BUDGET_DIVISOR: usize = 4;

/// Ring-crossing control-flow signal returned by [`TurnRun::attempt_entry`].
///
/// Every `break 'ring` / `break 'tool_loop`-then-rotate / early `return` that
/// used to thread across the old `run` maps to exactly one variant:
/// * `Continue`  — the attempt rotated within budget; drive the next ring entry.
/// * `BreakRing` — stop the ring (a final answer, the tool-turn cap, or the
///   rotation budget being spent); fall through to the post-ring checks.
/// * `Done`      — the turn already published its terminal event and ended the
///   span; the driver must return immediately (a former early `return`).
enum Flow {
    Continue,
    BreakRing,
    Done,
}

/// Owns all the mutable per-turn state that used to be locals inside `run`.
pub(crate) struct TurnRun {
    orch: TurnOrchestrator,
    session_id: String,
    turn_id: String,
    task: Task,
    turn_span: global::BoxedSpan,

    // Set by `route`.
    role: AgentRole,

    // Set by `build_context`.
    session: Option<Session>,
    workspace_root: std::path::PathBuf,
    base_system: String,
    all_tools: Vec<serde_json::Value>,
    budget_tokens: usize,
    mem: WorkingMemory,
    turn_correlation: CorrelationCtx,
    turn_cap: usize,
    turn_deadline: tokio::time::Instant,

    // Accumulated across the ring / tool loop.
    full_response: String,
    total_input_tokens: u32,
    total_output_tokens: u32,
    total_cache_read_tokens: u32,
    total_cold_omitted_tokens: usize,
    runner: String,
    model: String,
    last_kind: &'static str,
    rotations: u32,
    reached_final_answer: bool,
    turn_context_window: usize,
}

impl TurnOrchestrator {
    /// Execute a single agent turn: load task → route → call LLM → tool loop →
    /// persist response → checkpoint.
    ///
    /// All errors are handled internally; failures are published as
    /// [`TurnEvent::fail`] events and the task is marked `"failed"` in the
    /// ingot.  The function returns `()` rather than propagating, matching the
    /// existing `tokio::spawn` call sites.
    pub(crate) async fn run(self, session_id: String, turn_id: String) {
        let tracer = global::tracer("smedja");
        let mut turn_span = tracer.start(tel::SPAN_AGENT_INVOKE);

        // 1. Load the task to retrieve user content.
        let task = match self.ingot.get_task(&turn_id).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                warn!(turn_id = %turn_id, "task not found; dropping turn");
                self.dispatcher
                    .publish(TurnEvent::fail(&session_id, &turn_id, "task not found"));
                turn_span.set_status(SpanStatus::error("task not found"));
                turn_span.end();
                return;
            }
            Err(e) => {
                warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to load task");
                let reason = e.to_string();
                self.dispatcher
                    .publish(TurnEvent::fail(&session_id, &turn_id, &reason));
                turn_span.set_status(SpanStatus::error(reason));
                turn_span.end();
                return;
            }
        };

        let mut tr = TurnRun::new(self, session_id, turn_id, task, turn_span);

        // 2. Route this turn to a provider, then build the ordered rotation ring
        //    of eligible providers. `ring` borrows a locally-held pool `Arc` so
        //    it stays independent of `&mut tr` across the driver loop.
        let route = tr.route().await;
        let pool = Arc::clone(&tr.orch.pool);
        let ring = pool.eligible_ring(route.runner, route.tier);
        if ring.is_empty() {
            tr.fail_no_provider().await;
            return;
        }

        // 3./4. Assemble context (strata + graph + LSP + vault recall + cold +
        //       history + sealed prefix) and mark the task in-progress.
        tr.build_context(&route).await;

        // 5. Drive the turn over the eligible ring.
        for &entry in &ring {
            match tr.attempt_entry(entry, &route, &ring).await {
                Flow::Continue => {}
                Flow::BreakRing => break,
                Flow::Done => return,
            }
        }

        // 6.–10. Persist the response, record cost/savings/checkpoint/snapshot,
        //        compact on pressure, close the span, and publish completion.
        tr.finalize().await;
    }
}

impl TurnRun {
    fn new(
        orch: TurnOrchestrator,
        session_id: String,
        turn_id: String,
        task: Task,
        turn_span: global::BoxedSpan,
    ) -> Self {
        Self {
            orch,
            session_id,
            turn_id,
            task,
            turn_span,
            role: AgentRole::Orchestrator,
            session: None,
            workspace_root: std::path::PathBuf::new(),
            base_system: String::new(),
            all_tools: Vec::new(),
            budget_tokens: 0,
            mem: WorkingMemory::new(0),
            turn_correlation: CorrelationCtx::default(),
            turn_cap: 0,
            turn_deadline: tokio::time::Instant::now(),
            full_response: String::new(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read_tokens: 0,
            total_cold_omitted_tokens: 0,
            runner: String::new(),
            model: String::new(),
            last_kind: "request",
            rotations: 0,
            reached_final_answer: false,
            turn_context_window: 128_000,
        }
    }

    /// Route this turn: role → tier/model via the assayer, then apply any
    /// session runner override. Records the resolved role on `self`.
    async fn route(&mut self) -> Route {
        let ingot = &self.orch.ingot;
        let assayer = &self.orch.assayer;
        let session_id = self.session_id.clone();
        let turn_id = self.turn_id.clone();

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
        self.role = role;
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
        route
    }

    /// Terminal failure when the route yields no eligible provider (former R3).
    async fn fail_no_provider(&mut self) {
        let ingot = &self.orch.ingot;
        let dispatcher = &self.orch.dispatcher;
        let session_id = &self.session_id;
        let turn_id = &self.turn_id;
        let reason = "no LLM provider available; turn cannot execute".to_owned();
        warn!(session_id = %session_id, turn_id = %turn_id, "{reason}");
        let _ = ingot.update_task_status(turn_id, "failed").await;
        dispatcher.publish(TurnEvent::fail(session_id, turn_id, &reason));
        self.turn_span.set_status(SpanStatus::error(reason));
        self.turn_span.end();
    }

    /// Assemble the turn context: session + workspace + prompts + tools, then
    /// working-memory strata/graph/LSP/vault-recall/cold/history and the sealed
    /// prefix. Marks the task in-progress and primes the per-turn accumulators.
    #[allow(clippy::items_after_statements)] // inline `use` keeps the block self-documenting
    async fn build_context(&mut self, route: &Route) {
        let ingot = &self.orch.ingot;
        let vault = &self.orch.vault;
        let embedder = &self.orch.embedder;
        let session_id = self.session_id.clone();
        let turn_id = self.turn_id.clone();
        let task = self.task.clone();
        let role = self.role;

        // 3. Load session for workspace root, cowork mode, and task context.
        let session = { ingot.get_session(&session_id).await.ok().flatten() };

        self.turn_span
            .set_attribute(KeyValue::new(tel::CONV_ID, session_id.clone()));
        self.turn_span.set_attribute(KeyValue::new(
            tel::OPERATION_NAME,
            tel::OPERATION_INVOKE_AGENT,
        ));
        self.turn_span
            .set_attribute(KeyValue::new(tel::SESSION_ID, session_id.clone()));
        self.turn_span
            .set_attribute(KeyValue::new(tel::TURN_ID, turn_id.clone()));
        self.turn_span.set_attribute(KeyValue::new(
            tel::AGENT_NAME,
            session
                .as_ref()
                .and_then(|s| s.mode.as_deref())
                .unwrap_or("interactive")
                .to_owned(),
        ));

        let workspace_root = {
            // The session stores the resolved absolute canonical git root (see
            // session.create). Re-resolving is idempotent for that stored value
            // and repairs the fallback case (no stored root) by walking to the
            // enclosing repo, so index / query / injection all key one DB path.
            let start = session
                .as_ref()
                .and_then(|s| s.workspace_root.as_deref())
                .map_or_else(crate::common::workspace_root, |s| {
                    crate::common::resolve_active_repo(std::path::Path::new(s))
                });
            if !start.join(".git").exists() {
                tracing::warn!(
                    path = %start.display(),
                    "workspace does not contain .git; tool execution may be in wrong directory",
                );
            }
            start
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

        let (base_system, all_tools) = self
            .assemble_tools(&session, &workspace_root, &task_prefix, role)
            .await;

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

        let first_user_content = self.build_first_user_content(&workspace_root, role).await;

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

        // 4b. Mark in_progress.
        {
            if let Err(e) = ingot.update_task_status(&turn_id, "in_progress").await {
                warn!(turn_id = %turn_id, error = %e, "failed to mark task in_progress");
            }
        }

        // Correlation for streamed events: carry the turn span's trace/span ids
        // so deltas and tool events are linkable to the turn, not just Started.
        let turn_correlation = {
            let sc = self.turn_span.span_context();
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

        let turn_cap = self
            .orch
            .max_tool_turns
            .map_or_else(crate::common::effective_max_tool_turns, |n| n as usize);

        // Wall-clock deadline shared across all provider rotations and tool-loop
        // iterations for this turn.  Using a single `Instant` deadline (rather
        // than a fresh per-iteration duration) prevents the effective ceiling from
        // multiplying up to `MAX_TOOL_TURNS * 5 min`.
        let turn_deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(crate::common::effective_agent_timeout_s());

        self.session = session;
        self.workspace_root = workspace_root;
        self.base_system = base_system;
        self.all_tools = all_tools;
        self.budget_tokens = budget_tokens;
        self.mem = mem;
        self.turn_correlation = turn_correlation;
        self.turn_cap = turn_cap;
        self.turn_deadline = turn_deadline;
    }

    /// Drive one eligible ring entry: derive its `CallOptions`, run the inner
    /// tool loop, and classify the outcome into a [`Flow`] signal.
    #[allow(clippy::items_after_statements)] // inline `use` keeps moved blocks self-documenting
    async fn attempt_entry(
        &mut self,
        entry: &ProviderEntry,
        route: &Route,
        ring: &[&ProviderEntry],
    ) -> Flow {
        // Derive this entry's model, resume id, cache hint, and permission mode
        // into the `CallOptions`, recording the routed runner/model on `self`.
        let (mut opts, runner_enum, session_store_key, context_window) =
            self.prepare_call_options(entry, route).await;

        let ingot = &self.orch.ingot;
        let dispatcher = &self.orch.dispatcher;
        let provider_sessions = &self.orch.provider_sessions;
        let session_id = self.session_id.clone();
        let turn_id = self.turn_id.clone();
        let base_system = self.base_system.clone();
        let budget_tokens = self.budget_tokens;
        let turn_cap = self.turn_cap;
        let turn_deadline = self.turn_deadline;
        let turn_correlation = self.turn_correlation.clone();
        let provider = &entry.provider;

        // Classified failure that triggers a rotation to the next ring entry.
        // `None` means the attempt completed (success or fatal handled inline).
        let mut rotate: Option<(&'static str, String)> = None;

        'tool_loop: for _iteration in 0..turn_cap {
            // 5a. Stream LLM response with rate-limit retry.
            let (response_text, input_tokens, output_tokens, cache_read_tokens, native_session_id) = {
                let mut backoff_secs = RATE_LIMIT_BACKOFF_BASE_SECS;
                let mut attempt = 0u32;
                // Assemble the budgeted prompt and apply verbosity steering for
                // this iteration. The prompt always leads with the sealed prefix
                // and all hot turns; warm turns are included until the budget is
                // spent. The conciseness directive is appended above 60% fill.
                let (prompt, omitted) = self.mem.build_prompt_with_omitted(budget_tokens);
                self.total_cold_omitted_tokens =
                    self.total_cold_omitted_tokens.saturating_add(omitted);
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
                            tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
                            backoff_secs = (backoff_secs * 2).min(60);
                        }
                        Ok(Err(crate::common::DrainError::Rotatable { kind, .. })) => {
                            rotate = Some((kind, format!("rotatable provider failure: {kind}")));
                            break 'tool_loop;
                        }
                        Ok(Err(crate::common::DrainError::Other(reason))) => {
                            warn!(turn_id = %turn_id, error = %reason, "stream error during turn");
                            let _ = ingot.update_task_status(&turn_id, "failed").await;
                            dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
                            tel::set_span_error(&mut self.turn_span, "request", &reason, false);
                            self.turn_span.end();
                            return Flow::Done;
                        }
                        Err(_elapsed) => {
                            let reason = format!(
                                "turn deadline exceeded after {}s",
                                crate::common::effective_agent_timeout_s()
                            );
                            warn!(turn_id = %turn_id, "turn wall-clock deadline exceeded");
                            let _ = ingot.update_task_status(&turn_id, "failed").await;
                            dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
                            tel::set_span_error(&mut self.turn_span, "request", &reason, false);
                            self.turn_span.end();
                            return Flow::Done;
                        }
                    }
                }
            };
            if matches!(runner_enum, Runner::Claude | Runner::Codex) {
                if let Some(native_session_id) = native_session_id {
                    provider_sessions.lock().await.insert(
                        session_store_key.clone(),
                        ProviderSessionEntry::new(native_session_id),
                    );
                }
            }
            self.total_input_tokens = self.total_input_tokens.saturating_add(input_tokens);
            self.total_output_tokens = self.total_output_tokens.saturating_add(output_tokens);
            self.total_cache_read_tokens = self
                .total_cache_read_tokens
                .saturating_add(cache_read_tokens);

            // 5b. Parse tool calls from the response text.
            let tool_calls = crate::executor::parse_all_tool_calls(&response_text);

            if tool_calls.is_empty() {
                // 5f. No tool call — this is the final response.
                self.full_response = response_text;
                self.reached_final_answer = true;
                break 'tool_loop;
            }

            self.mem
                .push(AdapterMessage::assistant(response_text.clone()));

            if tool_calls.len() > 1 {
                // Multi-tool batch: read-only tools run concurrently; write/exec
                // tools run sequentially through the cowork gate. The whole
                // batch is bounded by the shared turn deadline so a hung tool
                // or a never-answered approval can't block the turn forever.
                let batch = self.run_multi_tool_batch(&tool_calls);

                let ordered_results = match tokio::time::timeout_at(turn_deadline, batch).await {
                    Ok(r) => r,
                    Err(_elapsed) => {
                        let reason = format!(
                            "turn deadline exceeded after {}s",
                            crate::common::effective_agent_timeout_s()
                        );
                        warn!(turn_id = %turn_id, "turn wall-clock deadline exceeded during tool batch");
                        let _ = ingot.update_task_status(&turn_id, "failed").await;
                        dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
                        tel::set_span_error(&mut self.turn_span, "request", &reason, false);
                        self.turn_span.end();
                        return Flow::Done;
                    }
                };

                // (c) Combine results in call order.
                use std::fmt::Write as _;
                let mut combined = String::new();
                for (i, (name, _)) in tool_calls.iter().enumerate() {
                    let crushed = smedja_adapter::crush::compress_tool_result(&ordered_results[i]);
                    let escaped = crushed.replace('<', "&lt;").replace('>', "&gt;");
                    let _ = writeln!(
                        combined,
                        "<tool_result tool=\"{name}\">{escaped}</tool_result>"
                    );
                }
                self.mem
                    .push(AdapterMessage::user(combined.trim_end().to_owned()));
                continue 'tool_loop;
            }

            // Single tool call: gate → execute → diagnostics → audit, then
            // append the compressed result as a user message and continue. A
            // tool call is deliberately NOT the turn's answer, so `full_response`
            // is left untouched — only the tool-free branch above records it.
            let (tool_name, tool_input) = tool_calls.into_iter().next().unwrap();
            let message = self.run_single_tool_call(&tool_name, tool_input).await;
            self.mem.push(AdapterMessage::user(message));
        }

        // Attempt finished. If no rotation was requested the turn either
        // produced a final answer or exhausted the tool-iteration cap.
        let Some((kind, message)) = rotate else {
            return Flow::BreakRing;
        };
        self.last_kind = kind;

        // Record the rotation on the turn span and emit a structured log line
        // naming the from/to runner and the classified kind.
        let to_runner = ring
            .get(self.rotations as usize + 1)
            .map_or("<none>", |e| e.runner_name);
        tel::set_span_error(&mut self.turn_span, kind, &message, true);
        self.turn_span.set_attribute(KeyValue::new(
            tel::ERROR_COUNT,
            i64::from(self.rotations + 1),
        ));
        let from_runner = self.runner.clone();
        warn!(
            turn_id = %turn_id,
            from_runner = %from_runner,
            to_runner,
            kind,
            rotation = self.rotations + 1,
            "rotating provider on retryable failure"
        );

        self.rotations += 1;
        if self.rotations > MAX_PROVIDER_ROTATIONS {
            // Rotation budget spent: stop and fail with the last kind below.
            return Flow::BreakRing;
        }
        // Otherwise the loop advances to the next eligible ring entry,
        // preserving the accumulated `WorkingMemory`.
        Flow::Continue
    }

    /// Assemble the turn's tool surface: the cacheable base system prompt (with
    /// workspace skills folded in), the built-in tool catalog (SRE-aware), and
    /// any MCP server tools. Also probes local-provider health as a side effect.
    async fn assemble_tools(
        &self,
        session: &Option<Session>,
        workspace_root: &std::path::Path,
        task_prefix: &str,
        role: AgentRole,
    ) -> (String, Vec<serde_json::Value>) {
        let ingot = &self.orch.ingot;

        // Base system prompt, with workspace skills folded into the stable
        // (cacheable) system block. Kept unsteered so verbosity steering can be
        // re-applied per tool-loop iteration without compounding.
        let base_system = prompt::build_base_system(workspace_root, task_prefix, role);

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

        let is_sre_mode = session
            .as_ref()
            .and_then(|s| s.mode.as_deref())
            .is_some_and(|m| m == "sre");
        let builtin_tools = tools_catalog::builtin_tools(is_sre_mode);

        let all_tools: Vec<serde_json::Value> =
            builtin_tools.into_iter().chain(mcp_tools).collect();
        (base_system, all_tools)
    }

    /// Derive the `CallOptions` for one ring entry: resolve the model
    /// (route/env/session override → entry default), project bundle subagents
    /// for claude-cli, look up the provider-native resume id, realise the
    /// cross-turn cache aligner hint, and fold in the session permission mode.
    /// Records the routed runner/model/context-window and span attributes on
    /// `self`, and returns `(opts, runner, session_store_key, context_window)`.
    async fn prepare_call_options(
        &mut self,
        entry: &ProviderEntry,
        route: &Route,
    ) -> (CallOptions, Runner, String, usize) {
        let gates = &self.orch.gates;
        let provider_sessions = &self.orch.provider_sessions;
        let cache_aligners = &self.orch.cache_aligners;
        let session = &self.session;
        let session_id = self.session_id.clone();
        let workspace_root = self.workspace_root.clone();
        let base_system = self.base_system.clone();
        let all_tools = &self.all_tools;

        let entry_runner_name = entry.runner_name.to_owned();
        let runner_enum = entry.runner;
        // Runner-agnostic subagents: claude-cli reads native subagent
        // definitions from `<workspace>/.claude/agents/`. Project the one
        // bundle's agent defs into that directory so the same folder that feeds
        // smedja's internal routing also reaches the native runner. Additive and
        // idempotent — a no-op when the bundle has no agents.
        if runner_enum == Runner::Claude {
            materialize_bundle_subagents(&workspace_root);
        }
        // Key the provider-native resume id by (session, runner), NOT by runner
        // alone. A bare runner key ("claude-cli") is global: the first turn's
        // native conversation id leaks into every other session, so later turns
        // pass `--resume <stale id>` and the CLI fails with "No conversation
        // found" (exit 1). Scoping by session id means a fresh session starts
        // with no resume and each session keeps its own conversation.
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
        self.turn_context_window = context_window;

        self.turn_span
            .set_attribute(KeyValue::new(tel::GEN_AI_SYSTEM, entry_runner_name.clone()));
        self.turn_span
            .set_attribute(KeyValue::new(tel::REQUEST_MODEL, entry_model.clone()));

        // Resolve the provider-native resume id from the NEW runner's session
        // key; a resume id from a previously-failed runner is never carried
        // across providers.
        let provider_session_id = if matches!(runner_enum, Runner::Claude | Runner::Codex) {
            // Refresh `last_used` on read so an active turn's entry is never
            // treated as idle by the background GC.
            provider_sessions
                .lock()
                .await
                .get_mut(&session_store_key)
                .map(|e| {
                    e.last_used = std::time::Instant::now();
                    e.id.clone()
                })
        } else {
            None
        };

        // Observe the sealed prefix for cross-turn drift using the aligner
        // persisted for THIS runner. Keying by `(session_id, runner)` keeps each
        // provider's prefix history separate: a `provider-failover` rotation to a
        // new runner finds no entry and starts fresh (its cache is cold), and
        // rotating back resumes that runner's recorded boundary. The lock is held
        // only for the take/align/re-insert and released before the provider call
        // below (no lock across `.await`).
        let aligner_key: AlignerKey = (session_id.clone(), entry_runner_name.clone());
        let cache_hint = {
            let mut aligners = cache_aligners.lock().await;
            let mut aligner = aligners.remove(&aligner_key).unwrap_or_default();
            let hint = aligner.align(&self.mem);
            aligners.insert(aligner_key, aligner);
            hint
        };

        // Realise the aligner hint for this runner: Anthropic via
        // `stable_prefix_len` (unchanged), OpenAI via stable-prefix ordering plus
        // a per-session cache key, Gemini via an optional context-cache handle
        // (lifecycle out of scope — none is supplied here, so Gemini falls back
        // to plain contents). Providers without prompt caching get no hint.
        let openai_cache_key = (entry_runner_name == "openai").then(|| session_id.clone());
        let (stable_prefix_len, cache_strategy) =
            cache_options_for_runner(&entry_runner_name, cache_hint, openai_cache_key, None);

        // The session's permission mode (default Ask), threaded so external CLIs
        // that can't gate per-tool (codex) can still map it to a sandbox level.
        let perm_mode = {
            let gate = gates.lock().await.get(&session_id).cloned();
            match gate {
                Some(g) => g.mode().await.as_str().to_owned(),
                None => crate::cowork::PermissionMode::default().as_str().to_owned(),
            }
        };

        let opts = CallOptions {
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

        self.runner = entry_runner_name.clone();
        self.model = entry_model.clone();
        (opts, runner_enum, session_store_key, context_window)
    }

    /// Build the sealed-prefix user turn: the task title plus auto-injected
    /// graph symbols, optional LSP diagnostics (code-focused turns only),
    /// auto-activated bundle skills, Unicode-tag sanitisation, and a leading
    /// per-turn context block (date + cwd).
    async fn build_first_user_content(
        &self,
        workspace_root: &std::path::Path,
        role: AgentRole,
    ) -> String {
        let mut content = self.task.title.clone();
        // Auto-inject top-3 graph symbols related to user message nouns.
        let stop_words = [
            "the", "and", "for", "with", "this", "that", "from", "into", "use", "are", "was",
            "has", "not", "can", "its", "will",
        ];
        let nouns: Vec<&str> = self
            .task
            .title
            .split_whitespace()
            .filter(|t| t.len() >= 3 && !stop_words.contains(&t.to_lowercase().as_str()))
            .take(5)
            .collect();
        let mut injected_count = 0usize;
        if !nouns.is_empty() {
            let graph_db_path = crate::handlers::graph::graph_db_path(workspace_root);
            if graph_db_path.exists() {
                let query = nouns.join(" ");
                // GraphStore open + query are blocking (SQLite) calls; run them
                // on the blocking pool so a tokio worker is never stalled.
                let snippets: Vec<String> = tokio::task::spawn_blocking(move || {
                    match smedja_graph::GraphStore::open(&graph_db_path) {
                        Ok(store) => match store.graph_query(&query, 3, 2) {
                            Ok(symbols) => symbols
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
                                .collect(),
                            Err(e) => {
                                tracing::debug!(error = %e, "graph_query failed; skipping injection");
                                Vec::new()
                            }
                        },
                        Err(e) => {
                            tracing::debug!(error = %e, "could not open graph.db; skipping injection");
                            Vec::new()
                        }
                    }
                })
                .await
                .unwrap_or_default();
                if !snippets.is_empty() {
                    let _ = write!(
                        content,
                        "\n\n<graph_symbols>\n{}\n</graph_symbols>",
                        snippets.join("\n\n")
                    );
                    injected_count = snippets.len();
                }
            } else {
                tracing::debug!("graph.db not found; skipping auto-injection");
            }
        }
        tracing::debug!(
            smedja.turn.graph_symbols_injected = injected_count,
            "graph symbol injection"
        );
        // Append LSP diagnostics only when the turn is code-focused: coder roles
        // or queries that mention fix/build/compile/error keywords.
        let wants_diag = matches!(role, AgentRole::Impl | AgentRole::Debug | AgentRole::Test)
            || ["fix", "build", "compile", "error", "warn"]
                .iter()
                .any(|kw| self.task.title.to_lowercase().contains(kw));
        if wants_diag {
            if let Some(diag_block) = format_lsp_diagnostics(&self.orch.lsp_manager.snapshot()) {
                let _ = write!(content, "\n\n{diag_block}");
            }
        }
        // Auto-activate relevant bundle skills: match the turn text and the
        // turn's touched files against skill triggers/paths, inlining the full
        // body of each selected skill for this turn only (the L1 index already
        // rode the stable prefix). Warn-free when nothing matches.
        {
            let touched: Vec<String> =
                crate::quality_hook::changed_file_sizes_for_review(workspace_root)
                    .into_iter()
                    .map(|(abs, _)| {
                        abs.strip_prefix(workspace_root)
                            .unwrap_or(&abs)
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect();
            if let Some(block) =
                prompt::build_selected_skills_block(workspace_root, &self.task.title, &touched)
            {
                let _ = write!(content, "\n\n{block}");
            }
        }
        // Sanitize Unicode tag block (U+E0000–U+E007F) to block prompt injection.
        let content = sanitize_unicode_tags(&content);
        // Prepend per-turn context block (date + cwd) for model orientation.
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let cwd_str = workspace_root.to_string_lossy();
        let turn_ctx = build_turn_context(&date, &cwd_str);
        format!("{turn_ctx}\n\n{content}")
    }

    /// Execute a multi-tool batch: read-only tools concurrently, write/exec
    /// tools sequentially through the cowork gate (with post-edit diagnostics),
    /// returning the results in the original call order. The caller bounds this
    /// by the shared turn deadline.
    async fn run_multi_tool_batch(&self, tool_calls: &[(String, String)]) -> Vec<String> {
        let ingot = &self.orch.ingot;
        let dispatcher = &self.orch.dispatcher;
        let gates = &self.orch.gates;
        let vault = &self.orch.vault;
        let embedder = &self.orch.embedder;
        let lsp_manager = &self.orch.lsp_manager;
        let session = &self.session;
        let session_id = &self.session_id;
        let turn_id = &self.turn_id;
        let role = self.role;
        let workspace_root = self.workspace_root.clone();
        let n = tool_calls.len();

        let mut ordered_results = vec![String::new(); n];

        // Partition into (original_index, name, input) for reads and writes.
        let (read_slots, write_slots): (Vec<_>, Vec<_>) = tool_calls
            .iter()
            .enumerate()
            .partition(|(_, (name, _))| crate::executor::READ_ONLY_TOOLS.contains(&name.as_str()));

        // (a) Parallel reads.
        {
            use futures_util::StreamExt as _;
            let mut futs = futures_util::stream::FuturesUnordered::new();
            for (i, (name, input)) in read_slots {
                let name = name.clone();
                let input = input.clone();
                let wsr = workspace_root.clone();
                let ig = ingot.clone();
                let vt = vault.clone();
                let em = Arc::clone(embedder);
                let lm = Arc::clone(lsp_manager);
                futs.push(async move {
                    let result =
                        execute_tool(&name, &input, &wsr, None, &ig, &vt, &em, Some(&lm)).await;
                    (i, result)
                });
            }
            while let Some((i, result)) = futs.next().await {
                ordered_results[i] = result;
            }
        }

        // (b) Sequential writes through the cowork gate.
        for (i, (name, input)) in write_slots {
            ordered_results[i] = if role.is_read_only() {
                format!(
                    "denied: the {} role is read-only and cannot run {name}",
                    role.label()
                )
            } else {
                let gate = {
                    let mut g = gates.lock().await;
                    Arc::clone(
                        g.entry(session_id.clone())
                            .or_insert_with(|| Arc::new(CoworkGate::default())),
                    )
                };
                let args_val = serde_json::from_str(input).unwrap_or(serde_json::Value::Null);
                // Thread `push` so a batch approval under Ask is surfaced to the
                // TUI (auto-approves in Auto mode); passing `None` here left the
                // approval invisible and the turn hung on the gate.
                let push = Some((dispatcher.as_ref(), Some(turn_id.as_str())));
                let decision = gate.gate_tool(0, name, args_val, "", push).await;
                match decision {
                    Decision::Approve => {
                        execute_tool(
                            name,
                            input,
                            &workspace_root,
                            session.as_ref(),
                            ingot,
                            vault,
                            embedder,
                            Some(lsp_manager),
                        )
                        .await
                    }
                    Decision::Deny(reason) => format!("denied: {reason}"),
                    Decision::Modify(new_input) => {
                        execute_tool(
                            name,
                            &new_input,
                            &workspace_root,
                            session.as_ref(),
                            ingot,
                            vault,
                            embedder,
                            Some(lsp_manager),
                        )
                        .await
                    }
                }
            };
            // Post-edit diagnostics feedback for batched writes.
            ordered_results[i] = append_edit_diagnostics(
                name,
                input,
                std::mem::take(&mut ordered_results[i]),
                lsp_manager,
            )
            .await;
        }

        ordered_results
    }

    /// Execute a single tool call end-to-end — publish `ToolCalled`, gate,
    /// run, post-edit diagnostics, ACP terminal status, audit — and return the
    /// compressed, escaped `<tool_result>` message to append to working memory.
    /// A tool call is never the turn's answer, so this touches no accumulator.
    async fn run_single_tool_call(&self, tool_name: &str, mut tool_input: String) -> String {
        let dispatcher = &self.orch.dispatcher;
        let lsp_manager = &self.orch.lsp_manager;
        let session = &self.session;
        let session_id = &self.session_id;
        let turn_id = &self.turn_id;
        let workspace_root = self.workspace_root.clone();

        let tool_call_id = Uuid::new_v4().to_string();

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
            tool_name: tool_name.to_owned(),
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

        // 5c./5d. Permission gate, then execute the tool (or take the denial).
        let tool_result = match self
            .gate_tool_call(tool_name, &tool_call_id, &mut tool_input)
            .await
        {
            Some(denial) => denial,
            None => {
                self.execute_tool_with_span(tool_name, &tool_input, &tool_call_id)
                    .await
            }
        };

        // Post-edit diagnostics feedback loop: after a successful edit, append
        // fresh language-server errors/warnings for the touched files so the
        // agent sees them without a separate build step.
        let tool_result =
            append_edit_diagnostics(tool_name, &tool_input, tool_result, lsp_manager).await;

        // ACP tool-call lifecycle: emit the terminal completed | failed status.
        // A successful edit carries a diff so an ACP client can render the
        // proposed change inline.
        {
            let status = tool_status_from_result(&tool_result);
            let content = if status == smedja_bellows::ToolCallStatus::Completed {
                tool_diff_content(&tool_input, &workspace_root).await
            } else {
                Vec::new()
            };
            dispatcher.publish(TurnEvent::ToolCallUpdate {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.to_owned(),
                status,
                content,
                turn_id: Some(turn_id.clone()),
                correlation: CorrelationCtx {
                    conversation_id: Some(session_id.clone()),
                    ..CorrelationCtx::default()
                },
            });
        }

        self.record_tool_audit(tool_name, &tool_call_id, &tool_result)
            .await;

        // 5e. Compress the tool result (SmartCrusher strips JSON nulls, bypassed
        // by SMEDJA_NO_TOOL_COMPRESS=1) before wrapping, so token budgeting
        // reflects the crushed size.
        let crushed = smedja_adapter::crush::compress_tool_result(&tool_result);
        let escaped_result = crushed.replace('<', "&lt;").replace('>', "&gt;");
        format!("<tool_result tool=\"{tool_name}\">{escaped_result}</tool_result>")
    }

    /// The 5c permission gate for a single tool call. Returns `Some(denial)`
    /// when the call must not run (read-only role, permission rule, or a cowork
    /// deny), or `None` to proceed — mutating `tool_input` in place when the
    /// gate returns a `Modify` decision.
    async fn gate_tool_call(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        tool_input: &mut String,
    ) -> Option<String> {
        let ingot = &self.orch.ingot;
        let dispatcher = &self.orch.dispatcher;
        let gates = &self.orch.gates;
        let session_id = &self.session_id;
        let turn_id = &self.turn_id;
        let role = self.role;
        let workspace_root = self.workspace_root.clone();

        if role.is_read_only()
            && crate::cowork::evaluate(crate::cowork::PermissionMode::Plan, tool_name)
                == crate::cowork::PermissionDecision::Deny
        {
            // Read-only roles (plan/research/review/ask/orchestrator) can never
            // mutate, regardless of the permission mode.
            return Some(format!(
                "denied: the {} role is read-only and cannot run {tool_name}",
                role.label()
            ));
        }
        let gate = {
            let mut g = gates.lock().await;
            Arc::clone(
                g.entry(session_id.clone())
                    .or_insert_with(|| Arc::new(CoworkGate::default())),
            )
        };
        let args_scrubbed = serde_json::from_str(tool_input).unwrap_or(serde_json::Value::Null);

        // Declarative permission rules take priority over session mode.
        let perm_rules = crate::cowork::load_permission_rules(&workspace_root);
        let rule_decision =
            crate::cowork::evaluate_permission_rules(&perm_rules, tool_name, &args_scrubbed);

        if matches!(rule_decision, Some(crate::cowork::PermissionDecision::Deny)) {
            // Deny before reaching the cowork gate; no audit event needed.
            return Some(format!(
                "denied: blocked by permission rule for {tool_name}"
            ));
        }
        // High-risk roles (IaC) always confirm a mutation — never auto-approved
        // even in Auto/AcceptEdits — because the ops (apply/destroy) are
        // dangerous and hard to reverse.
        let push = Some((dispatcher.as_ref(), Some(turn_id.as_str())));
        let gate_mode = gate.mode().await;
        let decision = if matches!(
            rule_decision,
            Some(crate::cowork::PermissionDecision::Allow)
        ) {
            Decision::Approve
        } else if role.is_high_risk()
            && crate::cowork::evaluate(crate::cowork::PermissionMode::Plan, tool_name)
                == crate::cowork::PermissionDecision::Deny
        {
            gate.gate_tool_forced_ask(0, tool_name, args_scrubbed, "", push)
                .await
        } else {
            gate.gate_tool(0, tool_name, args_scrubbed, "", push).await
        };
        // Record auto_approved when Auto mode bypassed a gate that Ask would have
        // held for human approval, so the audit trail shows the bypass.
        if matches!(&decision, Decision::Approve)
            && gate_mode == crate::cowork::PermissionMode::Auto
            && crate::cowork::evaluate(crate::cowork::PermissionMode::Ask, tool_name)
                == crate::cowork::PermissionDecision::Ask
        {
            let ev = smedja_ingot::AuditEvent {
                id: Uuid::new_v4(),
                ts: Timestamp::now(),
                session_id: session_id.clone(),
                turn_id: Some(turn_id.clone()),
                action_type: "auto_approved".into(),
                actor: "smdjad".into(),
                tool_name: Some(tool_name.to_owned()),
                tool_call_id: Some(tool_call_id.to_owned()),
                ..smedja_ingot::AuditEvent::default()
            };
            if let Err(e) = ingot.record_timeline_event(ev).await {
                warn!(
                    turn_id = %turn_id,
                    error = %e,
                    "failed to record auto_approved event"
                );
            }
        }
        match decision {
            Decision::Approve => None,
            Decision::Deny(reason) => Some(format!("denied: {reason}")),
            Decision::Modify(new_input) => {
                *tool_input = new_input;
                None
            }
        }
    }

    /// The 5d execution step: run the approved tool inside a
    /// `SPAN_TOOL_EXECUTE` span (emitting the ACP `InProgress` update) and
    /// return its raw result string.
    async fn execute_tool_with_span(
        &self,
        tool_name: &str,
        tool_input: &str,
        tool_call_id: &str,
    ) -> String {
        let dispatcher = &self.orch.dispatcher;
        let ingot = &self.orch.ingot;
        let vault = &self.orch.vault;
        let embedder = &self.orch.embedder;
        let lsp_manager = &self.orch.lsp_manager;
        let session = &self.session;
        let session_id = &self.session_id;
        let turn_id = &self.turn_id;
        let workspace_root = self.workspace_root.clone();
        let tracer = global::tracer("smedja");

        let tool_type_val = if crate::executor::LOCAL_TOOLS.contains(&tool_name) {
            if matches!(
                tool_name,
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
        tool_span.set_attribute(KeyValue::new(tel::TOOL_NAME, tool_name.to_owned()));
        tool_span.set_attribute(KeyValue::new(tel::TOOL_TYPE, tool_type_val));
        tool_span.set_attribute(KeyValue::new(tel::TOOL_CALL_ID, tool_call_id.to_owned()));
        match tel::tool_args_capture_mode() {
            tel::CaptureMode::Hash => {
                tool_span.set_attribute(KeyValue::new(
                    tel::TOOL_ARGS_HASH,
                    tel::content_hash(tool_input),
                ));
            }
            tel::CaptureMode::Scrubbed | tel::CaptureMode::Full => {
                tool_span.set_attribute(KeyValue::new(
                    tel::TOOL_ARGS_HASH,
                    tel::content_hash(tool_input),
                ));
                tool_span.set_attribute(KeyValue::new(
                    "gen_ai.tool.args",
                    tel::scrub_and_summarise(tool_input),
                ));
            }
        }
        // ACP tool-call lifecycle: the call moves pending → in_progress as
        // execution begins (the initial ToolCalled event is the pending start).
        dispatcher.publish(TurnEvent::ToolCallUpdate {
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            status: smedja_bellows::ToolCallStatus::InProgress,
            content: Vec::new(),
            turn_id: Some(turn_id.clone()),
            correlation: CorrelationCtx {
                conversation_id: Some(session_id.clone()),
                ..CorrelationCtx::default()
            },
        });
        let result = execute_tool(
            tool_name,
            tool_input,
            &workspace_root,
            session.as_ref(),
            ingot,
            vault,
            embedder,
            Some(lsp_manager),
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
            tool_span.set_status(opentelemetry::trace::Status::Ok);
        } else {
            tool_span.set_status(opentelemetry::trace::Status::error(
                result.chars().take(120).collect::<String>(),
            ));
        }
        tool_span.end();
        result
    }

    /// Persist a completed tool execution as a `tool_exec` timeline event.
    async fn record_tool_audit(&self, tool_name: &str, tool_call_id: &str, tool_result: &str) {
        let ingot = &self.orch.ingot;
        let session_id = &self.session_id;
        let turn_id = &self.turn_id;

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
            tool_name: Some(tool_name.to_owned()),
            traceparent: None,
            trace_id: t_trace_id,
            span_id: t_span_id,
            conversation_id: Some(session_id.clone()),
            tool_call_id: Some(tool_call_id.to_owned()),
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

    /// Persist the turn's ledgers: the billed cost entry, source-tagged token
    /// savings (cache reads + cold-context omission, zero-valued rows skipped),
    /// the message checkpoint, and the cumulative per-turn token snapshot. All
    /// errors are logged and swallowed — a ledger write must never break a turn.
    async fn persist_turn_records(&self, turn_n: i64) {
        let ingot = &self.orch.ingot;
        let price_table = &self.orch.price_table;
        let session_id = &self.session_id;
        let turn_id = &self.turn_id;

        // 7. Record cost entry.
        {
            let cost_usd = price_table.compute_cost(
                &self.model,
                self.total_input_tokens,
                self.total_output_tokens,
            );
            let entry = CostEntry {
                id: Uuid::new_v4(),
                session_id: session_id.clone(),
                turn_n,
                runner: self.runner.clone(),
                model: self.model.clone(),
                input_tok: i64::from(self.total_input_tokens),
                output_tok: i64::from(self.total_output_tokens),
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
        // estimate (source=cold-context). Zero-valued savings write no row.
        {
            if self.total_cache_read_tokens > 0 {
                let entry = TokensSavedEntry {
                    id: Uuid::new_v4(),
                    session_id: session_id.clone(),
                    turn_n,
                    command: "cache_read".to_owned(),
                    tokens_saved: i64::from(self.total_cache_read_tokens),
                    source: "cache".to_owned(),
                    created_at: Timestamp::now(),
                };
                if let Err(e) = ingot.insert_tokens_saved(entry).await {
                    warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to record cache savings");
                }
            }
            if self.total_cold_omitted_tokens > 0 {
                let entry = TokensSavedEntry {
                    id: Uuid::new_v4(),
                    session_id: session_id.clone(),
                    turn_n,
                    command: "cold_context".to_owned(),
                    tokens_saved: i64::try_from(self.total_cold_omitted_tokens).unwrap_or(i64::MAX),
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
            let messages_json_value: Vec<serde_json::Value> = self
                .mem
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
            let input_tok = i64::from(self.total_input_tokens);
            let output_tok = i64::from(self.total_output_tokens);
            let (prior_in, prior_out) =
                ingot
                    .session_token_snapshots(session_id)
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
    }

    /// Auto-summarise the conversation when context pressure exceeds the
    /// configured threshold: run a fast summariser, upsert the summary into the
    /// vault `compact` namespace, and publish a `HistoryReplaced` event. A no-op
    /// below threshold or when no summariser provider is available.
    async fn maybe_compact(&self, turn_n: i64) {
        let dispatcher = &self.orch.dispatcher;
        let session_id = &self.session_id;
        let turn_id = &self.turn_id;

        let cpt =
            compact_threshold_from_env(std::env::var("SMEDJA_COMPACT_THRESHOLD").ok().as_deref());
        if !context_pressure_exceeds_threshold(
            self.total_input_tokens,
            self.turn_context_window,
            cpt,
        ) {
            return;
        }
        let history: Vec<(String, String)> = self
            .mem
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
            .orch
            .pool
            .get(smedja_assayer::Runner::Claude, smedja_assayer::Tier::Fast)
            .or_else(|| {
                self.orch
                    .pool
                    .get(smedja_assayer::Runner::Codex, smedja_assayer::Tier::Fast)
            })
            .or_else(|| self.orch.pool.get_default());
        let Some(entry) = pool_entry else {
            return;
        };
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
        match crate::common::drain_stream(stream, &sum_dispatcher, None, &CorrelationCtx::default())
            .await
        {
            Ok((summary, _, _, _, _)) if !summary.is_empty() => {
                let summary_tokens = summary.split_whitespace().count();
                let embedding = self.orch.embedder.embed_query(&summary).await;
                let model_id = self.orch.embedder.model_id().to_owned();
                let dim = self.orch.embedder.dim();
                let compact_sid = session_id.clone();
                let vault = Arc::clone(&self.orch.vault);
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
                dispatcher.publish(smedja_bellows::TurnEvent::HistoryReplaced {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    summary_tokens,
                });
                tracing::info!(
                    session_id = %session_id,
                    turn_n,
                    threshold = cpt,
                    summary_tokens,
                    "auto-summarise: context compacted"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(error = %e, "auto-summarise: summariser call failed; continuing");
            }
        }
    }

    /// Finalise the turn after the ring: persist the response and mark complete,
    /// record cost/savings/checkpoint/token-snapshot, auto-summarise on context
    /// pressure, close the span, and publish the terminal audit + completion.
    #[allow(clippy::items_after_statements)] // inline `use` keeps moved blocks self-documenting
    async fn finalize(&mut self) {
        let ingot = &self.orch.ingot;
        let dispatcher = &self.orch.dispatcher;
        let session_id = self.session_id.clone();
        let turn_id = self.turn_id.clone();
        let task = self.task.clone();
        let session = &self.session;
        let turn_cap = self.turn_cap;

        // If every attempt rotated (the ring was exhausted or the rotation cap
        // was hit) the turn never produced a response: fail with the last kind.
        if self.full_response.is_empty() && self.rotations > 0 {
            let last_kind = self.last_kind;
            let reason = format!("all eligible providers failed; last error: {last_kind}");
            warn!(turn_id = %turn_id, last_kind, "provider ring exhausted");
            let _ = ingot.update_task_status(&turn_id, "failed").await;
            dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
            tel::set_span_error(&mut self.turn_span, last_kind, &reason, false);
            self.turn_span.end();
            return;
        }

        // The tool loop ran to its cap without the model ever returning a
        // tool-free reply: the last assistant text was raw tool-call JSON, not an
        // answer. Fail the turn (surfacing the exhaustion) instead of persisting
        // that JSON as if it were the final response.
        if !self.reached_final_answer {
            let reason = format!("tool-turn cap ({turn_cap}) reached before a final answer");
            warn!(turn_id = %turn_id, cap = turn_cap, "tool-turn cap exhausted without final answer");
            let _ = ingot.update_task_status(&turn_id, "failed").await;
            dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
            tel::set_span_error(&mut self.turn_span, "tool_cap_exhausted", &reason, false);
            self.turn_span.end();
            return;
        }

        // 6. Persist response and mark complete.
        if let Err(e) = ingot.set_task_response(&turn_id, &self.full_response).await {
            let reason = e.to_string();
            warn!(turn_id = %turn_id, error = %reason, "failed to store task response");
            dispatcher.publish(TurnEvent::fail(&session_id, &turn_id, &reason));
            self.turn_span.set_status(SpanStatus::error(reason));
            self.turn_span.end();
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

        // 7.–9. Record cost, source-tagged savings, checkpoint, and the per-turn
        // token snapshot.
        self.persist_turn_records(turn_n).await;

        // 9b. Auto-summarise when context pressure exceeds the configured threshold.
        self.maybe_compact(turn_n).await;

        self.turn_span.set_attribute(KeyValue::new(
            tel::INPUT_TOKENS,
            i64::from(self.total_input_tokens),
        ));
        self.turn_span.set_attribute(KeyValue::new(
            tel::OUTPUT_TOKENS,
            i64::from(self.total_output_tokens),
        ));
        self.turn_span
            .set_attribute(KeyValue::new(tel::TIER, self.runner.clone()));
        self.turn_span.set_attribute(KeyValue::new(
            "smedja.agent.kind",
            session
                .as_ref()
                .and_then(|s| s.mode.as_deref())
                .unwrap_or("impl")
                .to_owned(),
        ));

        let sc = self.turn_span.span_context().clone();
        let span_trace_id = format!("{}", sc.trace_id());
        let span_span_id = format!("{}", sc.span_id());
        let traceparent = format!("00-{span_trace_id}-{span_span_id}-01");

        self.turn_span.end();

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
                input_tok: i64::from(self.total_input_tokens),
                output_tok: i64::from(self.total_output_tokens),
                traceparent: Some(traceparent.clone()),
                trace_id: Some(span_trace_id),
                span_id: Some(span_span_id),
                conversation_id: Some(session_id.clone()),
                agent_name: Some(agent_name_val),
                operation_name: Some(tel::OPERATION_INVOKE_AGENT.to_owned()),
                status: Some("ok".to_owned()),
                change_name: self.orch.active_change.clone(),
                ..smedja_ingot::AuditEvent::default()
            };
            if let Err(e) = ingot.record_timeline_event(audit_ev).await {
                warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to record turn audit event");
            }
        }

        dispatcher.publish(TurnEvent::Completed {
            session_id: session_id.clone(),
            turn_id: turn_id.clone(),
            output_tokens: self.total_output_tokens,
            input_tokens: Some(self.total_input_tokens),
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

/// Project the active bundle's subagent definitions into
/// `<workspace>/.claude/agents/` so the native claude-cli runner sees the same
/// agents that feed smedja's internal routing. Additive and idempotent — a
/// no-op when the bundle declares no agents.
fn materialize_bundle_subagents(workspace_root: &std::path::Path) {
    let bundle = crate::bundle_config::load_bundle(workspace_root);
    // Log how each agent def binds to smedja's internal routing: a name matching
    // a built-in role refines that role's tool policy; an unmatched name is
    // materialised only for the native runner. This is the AgentRole mapping
    // surface for subagents.
    for agent in bundle.agents() {
        match crate::subagents::role_for_agent(&agent.name) {
            Some(role) => tracing::debug!(
                agent = %agent.name,
                role = role.label(),
                tools = crate::subagents::agent_tools(agent).len(),
                "bundle agent bound to role"
            ),
            None => tracing::debug!(
                agent = %agent.name,
                "bundle agent is native-only (no matching role)"
            ),
        }
    }
    let dest = workspace_root.join(".claude").join("agents");
    match crate::subagents::materialize_agents(&bundle, &dest) {
        Ok(0) => {}
        Ok(n) => tracing::debug!(count = n, "materialized bundle agents for claude-cli"),
        Err(e) => {
            tracing::warn!(error = %e, "failed to materialize bundle agents; continuing")
        }
    }
}
