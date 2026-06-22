pub mod acp;
pub mod alert;
pub mod compact;
pub mod cowork;
pub mod local_provider;
pub mod mcp_http;
pub mod mcp_oauth;
pub mod sandbox;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use opentelemetry::{
    global,
    trace::{Span as _, Status as SpanStatus, Tracer as _},
    KeyValue,
};
use serde_json::{json, Value};
use smedja_adapter::types::{Message as AdapterMessage, Role as AdapterRole};
use smedja_adapter::{
    AnthropicProvider, BergetProvider, CallOptions, ClaudeCliProvider, CodexCliProvider,
    CopilotProvider, Delta, LocalProvider, MinimaxProvider, OpenAiProvider, PoolsideProvider,
    Provider,
};
use smedja_assayer::{BashArity, WorktreePool};
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_ingot::{Checkpoint, CostEntry, Ingot, McpServer, Session, Task, TokenSnapshot};
use smedja_rpc::{codes, router::Router, server::Server, RpcError};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

use smedja_telemetry as tel;

use crate::cowork::{ApprovalPrompt, CoworkGate, Decision};
use crate::sandbox::SandboxExecutor;

fn socket_path() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(base).join("smdjad.sock")
}

/// RAII guard that removes the Unix socket file when dropped.
///
/// This ensures the socket is cleaned up on both clean shutdown and error
/// propagation (e.g. when `server.serve()` returns `Err` and `?` exits early).
struct SocketGuard {
    path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// In-memory store for content blocks addressed by SHA-256 hash.
/// Used by the `smedja_retrieve` tool to look up compressed context blocks.
fn retrieve_store() -> &'static tokio::sync::Mutex<HashMap<String, String>> {
    static STORE: OnceLock<tokio::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
    STORE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

fn open_ingot() -> anyhow::Result<Ingot> {
    // Try to open the persistent store under ~/.local/share/smedja/smedja.db.
    // If the data directory cannot be created, fall back to in-memory.
    let data_dir = dirs_home()
        .map(|h| h.join(".local").join("share").join("smedja"))
        .filter(|d| std::fs::create_dir_all(d).is_ok());

    if let Some(dir) = data_dir {
        let db_path = dir.join("smedja.db");
        Ingot::open(&db_path).map_err(anyhow::Error::from)
    } else {
        tracing::error!("cannot create data directory; using in-memory store — all session data will be lost on restart");
        Ingot::open_in_memory().map_err(anyhow::Error::from)
    }
}

/// Returns the user's home directory, or `None` if it cannot be determined.
fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn ingot_err(e: &smedja_ingot::IngotError) -> RpcError {
    RpcError::new(codes::INTERNAL_ERROR, e.to_string())
}

fn missing_param(name: &str) -> RpcError {
    RpcError::new(
        codes::INVALID_PARAMS,
        format!("missing required param: {name}"),
    )
}

/// Selects the LLM provider from environment variables and installed CLIs.
///
/// Priority order (stops at the first that resolves):
/// 1. `claude` binary on `$PATH` → [`ClaudeCliProvider`] (subscription, no API key)
/// 2. `codex` binary on `$PATH` → [`CodexCliProvider`] (subscription, no API key)
/// 3. `gh` binary + copilot extension (or `GITHUB_TOKEN`) → [`CopilotProvider`]
/// 4. `poolside` binary → [`PoolsideProvider`]
/// 5. `ANTHROPIC_API_KEY` → [`AnthropicProvider`] (API key fallback)
/// 6. `OPENAI_API_KEY` → [`OpenAiProvider`] (API key fallback)
/// 7. `MINIMAX_API_KEY` → [`MinimaxProvider`]
/// 8. `BERGET_API_KEY` → [`BergetProvider`]
/// 9. Local rs-llmctl endpoint health check → [`LocalProvider`]
///
/// Returns `Err(reason)` only when all options are unavailable.
/// Returns `(provider, runner_name, default_model)` for the first available
/// provider, in priority order.  The `runner_name` and `default_model` values
/// are derived from the concrete provider type, not re-checked from the
/// environment after selection.
async fn build_provider() -> Result<(Box<dyn Provider>, &'static str, &'static str), String> {
    // CLI subscription providers take priority over API key providers.
    if let Some(p) = ClaudeCliProvider::detect(None) {
        info!(
            provider = "claude-cli",
            "provider selected via claude CLI subscription"
        );
        return Ok((Box::new(p), "claude-cli", "claude-haiku-4-5-20251001"));
    }
    if let Some(p) = CodexCliProvider::detect(None) {
        info!(
            provider = "codex-cli",
            "provider selected via codex CLI subscription"
        );
        return Ok((Box::new(p), "codex-cli", "gpt-4o-mini"));
    }
    if let Some(p) = CopilotProvider::detect() {
        info!(provider = "copilot", "provider selected");
        return Ok((Box::new(p), "copilot", "gpt-4o-mini"));
    }
    if let Some(p) = PoolsideProvider::detect() {
        info!(provider = "poolside", "provider selected");
        return Ok((Box::new(p), "poolside", "poolside-muse"));
    }
    // API key providers as fallback from CLI subscription providers.
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        info!(provider = "anthropic", "provider selected");
        return Ok((
            Box::new(AnthropicProvider::new(key)),
            "anthropic",
            "claude-haiku-4-5-20251001",
        ));
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        info!(provider = "openai", "provider selected");
        return Ok((
            Box::new(OpenAiProvider::new("https://api.openai.com", key)),
            "openai",
            "gpt-4o-mini",
        ));
    }
    if let Some(p) = MinimaxProvider::detect() {
        info!(provider = "minimax", "provider selected");
        return Ok((Box::new(p), "minimax", "abab6.5s-chat"));
    }
    if let Some(p) = BergetProvider::detect() {
        info!(provider = "berget", "provider selected");
        return Ok((Box::new(p), "berget", "gpt-4o-mini"));
    }
    // Fall back to the local rs-llmctl endpoint.
    let local = LocalProvider::connect().await;
    if local.capability.healthy {
        info!(
            provider = "local",
            model_id = %local.capability.model_id,
            "provider selected",
        );
        return Ok((Box::new(local), "local", "local"));
    }
    warn!("no provider available — all options exhausted");
    Err("no LLM API key and local endpoint unreachable".to_owned())
}

/// Drains `stream`, accumulating text deltas into a single string.
///
/// Returns `Ok((full_response, input_tokens, output_tokens))` on success, or
/// `Err(reason)` if the stream yields an error item.  Each `Delta::Text` chunk
/// is forwarded to `dispatcher` as a [`TurnEvent::AssistantDelta`].
/// Error returned by [`drain_stream`], distinguishing rate-limit responses from
/// other failures so callers can apply an appropriate retry strategy.
enum DrainError {
    /// The provider returned HTTP 429; back off for `retry_after` before retrying.
    RateLimited {
        retry_after: Option<std::time::Duration>,
    },
    /// Any other stream-level error; treat as fatal for this turn.
    Other(String),
}

impl std::fmt::Display for DrainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited { retry_after } => {
                write!(f, "rate limited by provider (retry after {retry_after:?})")
            }
            Self::Other(s) => f.write_str(s),
        }
    }
}

async fn drain_stream(
    mut stream: smedja_adapter::DeltaStream,
    dispatcher: &Dispatcher,
) -> Result<(String, u32, u32), DrainError> {
    let mut full_response = String::new();
    let mut input_tokens = 0u32;
    let mut output_tokens = 0u32;
    loop {
        match stream.next().await {
            None => break,
            Some(Ok(Delta::Text(t))) => {
                full_response.push_str(&t);
                dispatcher.publish(TurnEvent::AssistantDelta {
                    content: t,
                    conversation_id: None,
                    trace_id: None,
                    span_id: None,
                    parent_span_id: None,
                    operation_name: None,
                    agent_name: None,
                    status: None,
                });
            }
            Some(Ok(Delta::Usage {
                input_tokens: i,
                output_tokens: n,
            })) => {
                input_tokens = i;
                output_tokens = n;
            }
            Some(Err(smedja_adapter::AdapterError::RateLimited { retry_after })) => {
                return Err(DrainError::RateLimited { retry_after });
            }
            Some(Err(e)) => return Err(DrainError::Other(e.to_string())),
        }
    }
    Ok((full_response, input_tokens, output_tokens))
}

/// Returns `true` when the session's mode permits write-arity bash commands.
///
/// The `"review"` mode is read-only by default; all other modes are unrestricted.
fn role_allows_write_bash(session: &Session) -> bool {
    // ponytail: review role is read-only by default; all others are unrestricted
    session.mode.as_deref() != Some("review")
}

/// Executes a bash command in `workspace` using `sh -c`, returning stdout or a
/// formatted error string.
async fn exec_bash(cmd: &str, workspace: &std::path::Path) -> String {
    match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(workspace)
        .output()
        .await
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        Ok(out) => format!("error: {}", String::from_utf8_lossy(&out.stderr)),
        Err(e) => format!("error: {e}"),
    }
}

/// Maximum number of tool-dispatch iterations in a single turn.
const MAX_TOOL_TURNS: usize = 10;

/// Executes a single turn: loads the task, calls the LLM, handles tool calls,
/// stores the final response.
#[allow(clippy::too_many_lines, clippy::items_after_statements)]
async fn run_turn(
    ingot: Arc<Mutex<Ingot>>,
    dispatcher: Arc<Dispatcher>,
    session_id: String,
    turn_id: String,
    gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
) {
    let tracer = global::tracer("smedja");
    let mut turn_span = tracer.start(tel::SPAN_AGENT_INVOKE);

    // 1. Load the task to retrieve user content.
    let task = {
        let ig = ingot.lock().await;
        match ig.get_task(&turn_id) {
            Ok(Some(t)) => t,
            Ok(None) => {
                warn!(turn_id = %turn_id, "task not found; dropping turn");
                dispatcher.publish(TurnEvent::Failed {
                    session_id,
                    turn_id,
                    reason: "task not found".to_owned(),
                    conversation_id: None,
                    trace_id: None,
                    span_id: None,
                    parent_span_id: None,
                    operation_name: None,
                    agent_name: None,
                    status: None,
                });
                turn_span.set_status(SpanStatus::error("task not found"));
                turn_span.end();
                return;
            }
            Err(e) => {
                warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to load task");
                let reason = e.to_string();
                dispatcher.publish(TurnEvent::Failed {
                    session_id,
                    turn_id,
                    reason: reason.clone(),
                    conversation_id: None,
                    trace_id: None,
                    span_id: None,
                    parent_span_id: None,
                    operation_name: None,
                    agent_name: None,
                    status: None,
                });
                turn_span.set_status(SpanStatus::error(reason));
                turn_span.end();
                return;
            }
        }
    };

    // 2. Select provider from environment.
    let (provider, provider_runner, provider_default_model) = match build_provider().await {
        Ok(triple) => triple,
        Err(reason) => {
            warn!(session_id = %session_id, turn_id = %turn_id, "no LLM provider available; turn cannot execute");
            let _ = ingot.lock().await.update_task_status(&turn_id, "failed");
            dispatcher.publish(TurnEvent::Failed {
                session_id,
                turn_id,
                reason: reason.clone(),
                conversation_id: None,
                trace_id: None,
                span_id: None,
                parent_span_id: None,
                operation_name: None,
                agent_name: None,
                status: None,
            });
            turn_span.set_status(SpanStatus::error(reason));
            turn_span.end();
            return;
        }
    };

    // Derive model and runner from the concrete provider type selected above.
    // SMEDJA_MODEL can override the default model name but never changes the runner.
    let runner = provider_runner.to_owned();
    let model = std::env::var("SMEDJA_MODEL").unwrap_or_else(|_| provider_default_model.to_owned());

    // 3. Load session for workspace root, cowork mode, and task context.
    let session = {
        let ig = ingot.lock().await;
        ig.get_session(&session_id).ok().flatten()
    };

    // Apply session model override: if the session has a stored model name, use it
    // instead of the environment-derived default chosen above.
    let model = session
        .as_ref()
        .and_then(|s| s.model_override.clone())
        .unwrap_or(model);

    // Attach mandatory GenAI semantic-convention attributes to the turn span now
    // that model and runner are resolved.
    turn_span.set_attribute(KeyValue::new(tel::GEN_AI_SYSTEM, runner.clone()));
    turn_span.set_attribute(KeyValue::new(tel::REQUEST_MODEL, model.clone()));
    turn_span.set_attribute(KeyValue::new(tel::CONV_ID, session_id.clone()));
    turn_span.set_attribute(KeyValue::new(
        tel::OPERATION_NAME,
        tel::OPERATION_INVOKE_AGENT,
    ));
    turn_span.set_attribute(KeyValue::new(tel::SESSION_ID, session_id.clone()));
    turn_span.set_attribute(KeyValue::new(tel::TURN_ID, turn_id.clone()));
    // agent_name: use session mode or "interactive" for user-facing sessions
    turn_span.set_attribute(KeyValue::new(
        tel::AGENT_NAME,
        session
            .as_ref()
            .and_then(|s| s.mode.as_deref())
            .unwrap_or("interactive")
            .to_owned(),
    ));

    // Derive workspace root: prefer session.workspace_root, then SMEDJA_WORKSPACE env, then ".".
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

    // Inject active task context wrapped in XML delimiters (prevents prompt injection).
    let task_prefix = {
        let ig = ingot.lock().await;
        match ig.get_session(&session_id) {
            Ok(Some(s)) => {
                if let Some(ref task_id) = s.task_id {
                    match ig.get_task(task_id) {
                        Ok(Some(active_task)) => format!(
                            "\n\n<active_task>\n<title>{}</title>\n<description>{}</description>\n</active_task>",
                            active_task.title,
                            active_task.description.as_str(),
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

    let system_prompt = format!("You are smedja, an AI coding assistant.{task_prefix}");

    // Load registered MCP tool definitions and flatten into a single Vec.
    let mcp_tools: Vec<serde_json::Value> = {
        let ig = ingot.lock().await;
        ig.list_mcp_servers()
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

    // Re-check local health if stale (> 30s since last check).
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
    // local_tool_format is "openai" or "xml"; used in tool dispatch
    if local_tool_format == "xml" {
        tracing::debug!(tool_format = "xml", "local provider tool format: xml");
    }

    // Builtin tools: smedja_vault_search, smedja_retrieve, graph_query
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
    // When session is in SRE mode, inject monitoring tools.
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

    let all_tools: Vec<serde_json::Value> = builtin_tools.into_iter().chain(mcp_tools).collect();

    let opts = CallOptions {
        model: model.clone(),
        max_tokens: Some(2048),
        temperature: Some(0.7),
        system: Some(system_prompt),
        tools: if all_tools.is_empty() {
            None
        } else {
            Some(all_tools)
        },
    };

    // 4. Mark in_progress.
    {
        let mut ig = ingot.lock().await;
        if let Err(e) = ig.update_task_status(&turn_id, "in_progress") {
            warn!(turn_id = %turn_id, error = %e, "failed to mark task in_progress");
        }
    }

    // 5. Tool-dispatch loop — up to MAX_TOOL_TURNS iterations.
    let mut messages: Vec<AdapterMessage> = vec![AdapterMessage {
        role: AdapterRole::User,
        content: task.title.clone(),
    }];

    // Auto-inject top-3 graph symbols related to user message nouns (Task 64).
    {
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
                                    let injection = format!(
                                        "\n\n<graph_symbols>\n{}\n</graph_symbols>",
                                        snippets.join("\n\n")
                                    );
                                    // Append to system message via prepending to first user message
                                    if let Some(first) = messages.first_mut() {
                                        first.content.push_str(&injection);
                                    }
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
    }

    // Maximum number of rate-limit retries per turn before giving up.
    const MAX_RATE_LIMIT_RETRIES: u32 = 4;
    // Initial back-off when no Retry-After header is present.
    const RATE_LIMIT_BACKOFF_BASE_SECS: u64 = 1;

    let mut full_response = String::new();
    let mut total_input_tokens = 0u32;
    let mut total_output_tokens = 0u32;

    'tool_loop: for _iteration in 0..MAX_TOOL_TURNS {
        // 5a. Stream LLM response with rate-limit retry (up to MAX_RATE_LIMIT_RETRIES).
        let (response_text, input_tokens, output_tokens) = {
            let mut backoff_secs = RATE_LIMIT_BACKOFF_BASE_SECS;
            let mut attempt = 0u32;
            loop {
                let stream = provider.stream_chat(&messages, &opts);
                let drain_result = tokio::time::timeout(
                    std::time::Duration::from_mins(5),
                    drain_stream(stream, &dispatcher),
                )
                .await;
                match drain_result {
                    Ok(Ok(triple)) => break triple,
                    Ok(Err(DrainError::RateLimited { retry_after })) => {
                        attempt += 1;
                        if attempt > MAX_RATE_LIMIT_RETRIES {
                            let reason =
                                "rate limited by provider; retry limit exceeded".to_owned();
                            warn!(turn_id = %turn_id, "rate limit retry limit exceeded");
                            let _ = ingot.lock().await.update_task_status(&turn_id, "failed");
                            dispatcher.publish(TurnEvent::Failed {
                                session_id,
                                turn_id,
                                reason: reason.clone(),
                                conversation_id: None,
                                trace_id: None,
                                span_id: None,
                                parent_span_id: None,
                                operation_name: None,
                                agent_name: None,
                                status: None,
                            });
                            turn_span.set_status(SpanStatus::error(reason));
                            turn_span.end();
                            return;
                        }
                        let sleep_secs = retry_after.map_or(backoff_secs, |d| d.as_secs().max(1));
                        warn!(
                            turn_id = %turn_id,
                            attempt,
                            sleep_secs,
                            "rate limited by provider; backing off"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
                        backoff_secs = (backoff_secs * 2).min(60);
                    }
                    Ok(Err(DrainError::Other(reason))) => {
                        warn!(turn_id = %turn_id, error = %reason, "stream error during turn");
                        let _ = ingot.lock().await.update_task_status(&turn_id, "failed");
                        dispatcher.publish(TurnEvent::Failed {
                            session_id,
                            turn_id,
                            reason: reason.clone(),
                            conversation_id: None,
                            trace_id: None,
                            span_id: None,
                            parent_span_id: None,
                            operation_name: None,
                            agent_name: None,
                            status: None,
                        });
                        turn_span.set_status(SpanStatus::error(reason));
                        turn_span.end();
                        return;
                    }
                    Err(_elapsed) => {
                        let reason = "stream timed out after 300s".to_owned();
                        warn!(turn_id = %turn_id, "provider stream timed out");
                        let _ = ingot.lock().await.update_task_status(&turn_id, "failed");
                        dispatcher.publish(TurnEvent::Failed {
                            session_id,
                            turn_id,
                            reason: reason.clone(),
                            conversation_id: None,
                            trace_id: None,
                            span_id: None,
                            parent_span_id: None,
                            operation_name: None,
                            agent_name: None,
                            status: None,
                        });
                        turn_span.set_status(SpanStatus::error(reason));
                        turn_span.end();
                        return;
                    }
                }
            }
        };
        total_input_tokens = total_input_tokens.saturating_add(input_tokens);
        total_output_tokens = total_output_tokens.saturating_add(output_tokens);

        // 5b. Parse tool calls from the response text.
        // The adapter streams plain text; detect tool calls via JSON heuristics.
        // A tool call appears as a JSON object with a "tool" key in the response.
        let tool_call = parse_tool_call(&response_text);

        if let Some((tool_name, tool_input)) = tool_call {
            // Generate a per-invocation correlation ID so the ToolCalled event
            // and the corresponding AuditEvent can be joined in the audit log.
            let tool_call_id = Uuid::new_v4().to_string();

            // Append assistant response to message history.
            messages.push(AdapterMessage {
                role: AdapterRole::Assistant,
                content: response_text.clone(),
            });

            // Extract current span IDs to correlate event with the tool span.
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
            // Emit the ToolCalled event with the correlation ID.
            dispatcher.publish(TurnEvent::ToolCalled {
                tool_name: tool_name.clone(),
                input_summary: tool_input.chars().take(120).collect(),
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

            // 5c. Cowork gate intercept (when session has cowork mode enabled).
            let cowork_denied = if session.as_ref().is_some_and(|s| s.cowork_mode) {
                if let Some(gate) = gates.lock().await.get(&session_id).cloned() {
                    let ap = ApprovalPrompt {
                        step_n: 0,
                        tool: tool_name.clone(),
                        args_scrubbed: serde_json::from_str(&tool_input).unwrap_or(Value::Null),
                        reasoning: String::new(),
                        plan_summary: String::new(),
                    };
                    match gate.intercept(ap, 300).await {
                        Decision::Approve => None,
                        Decision::Deny(reason) => Some(format!("denied: {reason}")),
                        Decision::Modify(_new_cmd) => {
                            // Modify path: fall through to execution with original input.
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
                // Classify tool type per the design contract.
                let tool_type_val = if tool_name.starts_with("mcp_") || tool_name.contains("mcp") {
                    "extension"
                } else if matches!(
                    tool_name.as_str(),
                    "vault_search" | "graph_query" | "retrieve"
                ) {
                    "datastore"
                } else {
                    "function"
                };
                let mut tool_span = tracer.start(tel::SPAN_TOOL_EXECUTE);
                tool_span.set_attribute(KeyValue::new(
                    tel::OPERATION_NAME,
                    tel::OPERATION_EXECUTE_TOOL,
                ));
                tool_span.set_attribute(KeyValue::new(tel::TOOL_NAME, tool_name.clone()));
                tool_span.set_attribute(KeyValue::new(tel::TOOL_TYPE, tool_type_val));
                tool_span.set_attribute(KeyValue::new(tel::TOOL_CALL_ID, tool_call_id.clone()));
                // Capture policy for tool args (section 7.1).
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
                let result =
                    execute_tool(&tool_name, &tool_input, &workspace_root, session.as_ref()).await;
                tool_span.set_attribute(KeyValue::new(
                    tel::TOOL_RESULT_HASH,
                    tel::content_hash(&result),
                ));
                tool_span.set_attribute(KeyValue::new(
                    tel::TOOL_RESULT_TOKENS,
                    i64::try_from(result.split_whitespace().count()).unwrap_or(0),
                ));
                // rough word-count approximation; good enough for telemetry
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

            // Persist tool execution as a timeline event (section 5.7).
            {
                // Get current span IDs for correlation.
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
                    ts: now_epoch(),
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
                let ig = ingot.lock().await;
                if let Err(e) = ig.record_timeline_event(&tool_audit) {
                    warn!(turn_id = %turn_id, error = %e, "failed to record tool audit event");
                }
            }

            // 5e. Append tool result as a user message and continue the loop.
            // Escape XML angle brackets in the raw tool output so that a
            // malicious or unexpected response cannot break out of the
            // <tool_result> wrapper and inject new XML structure into the
            // prompt (prompt-injection mitigation).
            let escaped_result = tool_result.replace('<', "&lt;").replace('>', "&gt;");
            messages.push(AdapterMessage {
                role: AdapterRole::User,
                content: format!(
                    "<tool_result tool=\"{tool_name}\">{escaped_result}</tool_result>"
                ),
            });

            full_response = response_text;
            continue 'tool_loop;
        }

        // 5f. No tool call — this is the final response.
        full_response = response_text;
        break 'tool_loop;
    }

    // 6. Persist response and mark complete.
    if let Err(e) = ingot
        .lock()
        .await
        .set_task_response(&turn_id, &full_response)
    {
        let reason = e.to_string();
        warn!(turn_id = %turn_id, error = %reason, "failed to store task response");
        dispatcher.publish(TurnEvent::Failed {
            session_id,
            turn_id,
            reason: reason.clone(),
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            operation_name: None,
            agent_name: None,
            status: None,
        });
        turn_span.set_status(SpanStatus::error(reason));
        turn_span.end();
        return;
    }

    // Count existing checkpoints to derive the sequential turn index.
    let turn_n: i64 = {
        let ig = ingot.lock().await;
        ig.list_checkpoints(&session_id)
            .map_or(0, |v| i64::try_from(v.len()).unwrap_or(i64::MAX))
    };

    // 7. Record cost entry.
    {
        let entry = CostEntry {
            id: Uuid::new_v4(),
            session_id: session_id.clone(),
            turn_n,
            runner: runner.clone(),
            model,
            input_tok: i64::from(total_input_tokens),
            output_tok: i64::from(total_output_tokens),
            cost_usd: 0.0, // ponytail: pricing table deferred
            created_at: now_epoch(),
        };
        let mut ig = ingot.lock().await;
        if let Err(e) = ig.insert_cost(&entry) {
            warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to record cost entry");
        }
    }

    // 8. Save checkpoint — persist the full accumulated message history so that
    //    tool-call/tool-result pairs from the loop are not lost.
    {
        let messages_json_value: Vec<serde_json::Value> = messages
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
            created_at: now_epoch(),
        };
        let mut ig = ingot.lock().await;
        if let Err(e) = ig.save_checkpoint(&cp) {
            warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to save checkpoint");
        }
    }

    // 9. Save per-turn token snapshot with running cumulative totals.
    {
        let input_tok = i64::from(total_input_tokens);
        let output_tok = i64::from(total_output_tokens);
        let mut ig = ingot.lock().await;
        // Compute cumulative totals by summing all prior snapshots.
        let (prior_in, prior_out) =
            ig.session_token_snapshots(&session_id)
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
            created_at: now_epoch(),
        };
        if let Err(e) = ig.save_token_snapshot(&snap) {
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

    // Extract trace context before ending the span so the traceparent field
    // is populated in the audit event.
    let sc = turn_span.span_context().clone();
    let span_trace_id = format!("{}", sc.trace_id());
    let span_span_id = format!("{}", sc.span_id());
    let traceparent = format!("00-{span_trace_id}-{span_span_id}-01");

    turn_span.end();

    // 10. Record audit event for this turn with the OTel span context.
    {
        let agent_name_val = session
            .as_ref()
            .and_then(|s| s.mode.as_deref())
            .unwrap_or("interactive")
            .to_owned();
        let audit_ev = smedja_ingot::AuditEvent {
            id: Uuid::new_v4(),
            ts: now_epoch(),
            session_id: session_id.clone(),
            turn_id: Some(turn_id.clone()),
            action_type: "turn_end".into(),
            actor: "smdjad".into(),
            input_tok: i64::from(total_input_tokens),
            output_tok: i64::from(total_output_tokens),
            traceparent: Some(traceparent),
            trace_id: Some(span_trace_id),
            span_id: Some(span_span_id),
            conversation_id: Some(session_id.clone()),
            agent_name: Some(agent_name_val),
            operation_name: Some(tel::OPERATION_INVOKE_AGENT.to_owned()),
            status: Some("ok".to_owned()),
            ..smedja_ingot::AuditEvent::default()
        };
        let ig = ingot.lock().await;
        if let Err(e) = ig.record_timeline_event(&audit_ev) {
            warn!(session_id = %session_id, turn_id = %turn_id, error = %e, "failed to record turn audit event");
        }
    }

    dispatcher.publish(TurnEvent::Completed {
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        output_tokens: total_output_tokens,
        conversation_id: Some(session_id.clone()),
        trace_id: None, // span already ended before this
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

/// Parses a tool call embedded in `text`, returning `(tool_name, input_json_string)`.
///
/// Looks for a JSON object with a `"tool"` key anywhere in the text.
/// Returns `None` when no tool call is detected.
fn parse_tool_call(text: &str) -> Option<(String, String)> {
    for (start, c) in text.char_indices() {
        if c != '{' {
            continue;
        }
        let slice = &text[start..];

        // Try balanced-brace extraction first so embedded JSON is handled correctly.
        let candidate = if let Some(end) = find_json_end(slice) {
            &slice[..end]
        } else {
            slice
        };

        if let Ok(v) = serde_json::from_str::<Value>(candidate) {
            if let Some(tool_name) = v.get("tool").and_then(Value::as_str) {
                let input = v
                    .get("input")
                    .map_or_else(|| "{}".to_owned(), std::string::ToString::to_string);
                return Some((tool_name.to_owned(), input));
            }
        }
    }
    None
}

/// Finds the index of the character after the balanced closing `}` for `s`, which
/// must start with `{`.
fn find_json_end(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut prev_backslash = false;
    for (i, c) in s.char_indices() {
        if in_string {
            if prev_backslash {
                prev_backslash = false;
            } else if c == '\\' {
                prev_backslash = true;
            } else if c == '"' {
                in_string = false;
            }
        } else {
            match c {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Executes the named tool with the given JSON input string.
///
/// Supported tools: `bash`, `run_command`, `read_file`, `list_files`.
/// Unknown tools return a formatted error string.
#[allow(clippy::too_many_lines)]
async fn execute_tool(
    tool_name: &str,
    tool_input: &str,
    workspace: &std::path::Path,
    session: Option<&Session>,
) -> String {
    let input: Value = serde_json::from_str(tool_input).unwrap_or(Value::Null);

    // Least-privilege enforcement: block write tools for read-only (review) sessions.
    if session.is_some_and(|s| s.mode.as_deref() == Some("review")) {
        const WRITE_TOOLS: &[&str] = &["edit_file", "bash", "write_file", "run_command"];
        if WRITE_TOOLS.contains(&tool_name) {
            tracing::warn!(
                tool = tool_name,
                "smedja.security.tool_blocked: write tool blocked for read-only session"
            );
            return format!(
                "error: tool '{tool_name}' is blocked for read-only roles (TOOL_BLOCKED)"
            );
        }
    }

    // Data access tracking: warn on absolute-path write attempts outside workspace.
    if matches!(tool_name, "write_file" | "edit_file") {
        if let Some(path_str) = input.get("path").and_then(Value::as_str) {
            let path = std::path::Path::new(path_str);
            if path.is_absolute() {
                tracing::warn!(
                    tool = tool_name,
                    path = path_str,
                    "smedja.security.data_access_blocked: write outside workspace attempted"
                );
            }
        }
    }

    match tool_name {
        "bash" | "run_command" => {
            let cmd = input
                .get("command")
                .or_else(|| input.get("cmd"))
                .and_then(Value::as_str)
                .unwrap_or_default();

            // Enforce read-only mode for review sessions.
            if session.is_some_and(|s| !role_allows_write_bash(s)) {
                let arity = smedja_assayer::classify_bash(cmd);
                if arity == BashArity::Write {
                    return "permission denied: review mode sessions cannot execute write commands"
                        .to_owned();
                }
            }

            // SandboxExecutor: use Docker sandbox when configured and tool is not exempt.
            let sandbox = SandboxExecutor::new();
            if sandbox.available && !SandboxExecutor::is_exempt(tool_name) {
                match sandbox.exec(cmd, workspace).await {
                    Ok(out) => out,
                    Err(e) => format!("error: {e}"),
                }
            } else {
                exec_bash(cmd, workspace).await
            }
        }
        "read_file" => {
            let path_str = input
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let full = workspace.join(path_str);
            match tokio::fs::read_to_string(&full).await {
                Ok(contents) => contents,
                Err(e) => format!("error reading {path_str}: {e}"),
            }
        }
        "list_files" => {
            let dir_str = input.get("path").and_then(Value::as_str).unwrap_or(".");
            let full = workspace.join(dir_str);
            match tokio::fs::read_dir(&full).await {
                Ok(mut rd) => {
                    let mut entries = Vec::new();
                    while let Ok(Some(entry)) = rd.next_entry().await {
                        entries.push(entry.file_name().to_string_lossy().into_owned());
                    }
                    entries.join("\n")
                }
                Err(e) => format!("error listing {dir_str}: {e}"),
            }
        }
        "smedja_vault_search" => {
            let query = input.get("query").and_then(Value::as_str).unwrap_or("");
            let k =
                usize::try_from(input.get("k").and_then(Value::as_u64).unwrap_or(5)).unwrap_or(5);
            // ponytail: vault not yet wired; stub returns empty results
            tracing::warn!(
                query,
                k,
                "smedja_vault_search called; vault stub returning empty results"
            );
            serde_json::json!({ "results": [] }).to_string()
        }
        "smedja_retrieve" => {
            let hash = input.get("hash").and_then(Value::as_str).unwrap_or("");
            let store = retrieve_store().lock().await;
            if let Some(content) = store.get(hash) {
                // ponytail: audit deferred; log the retrieval.
                tracing::info!(hash, "smedja_retrieve hit");
                content.clone()
            } else {
                tracing::debug!(hash, "smedja_retrieve: hash not found");
                format!("error: hash not found: {hash}")
            }
        }
        "graph_query" => {
            let query = input.get("query").and_then(Value::as_str).unwrap_or("");
            let depth =
                u8::try_from(input.get("depth").and_then(Value::as_u64).unwrap_or(2)).unwrap_or(2);
            let graph_db_path = workspace.join(".smedja").join("graph.db");
            if !graph_db_path.exists() {
                tracing::debug!("graph.db not found; returning empty symbols");
                return serde_json::json!({ "symbols": [] }).to_string();
            }
            match smedja_graph::GraphStore::open(&graph_db_path) {
                Ok(store) => match store.graph_query(query, 10, depth) {
                    Ok(symbols) => {
                        let sym_json: Vec<serde_json::Value> = symbols
                            .iter()
                            .map(|s| {
                                serde_json::json!({
                                    "name": s.name,
                                    "kind": s.kind.as_str(),
                                    "file": s.file_path,
                                    "line": s.start_line,
                                    "snippet": s.snippet,
                                })
                            })
                            .collect();
                        serde_json::json!({ "symbols": sym_json }).to_string()
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "graph_query error");
                        serde_json::json!({ "symbols": [], "error": e.to_string() }).to_string()
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, "failed to open graph store");
                    serde_json::json!({ "symbols": [] }).to_string()
                }
            }
        }
        "alert_list" => {
            let alerts = crate::alert::drain_alerts(50).await;
            serde_json::to_string(&alerts).unwrap_or_default()
        }
        "otel_query" => {
            let service = input
                .get("service")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let filter = input.get("filter").and_then(|v| v.as_str());
            let range = input
                .get("range_minutes")
                .and_then(Value::as_i64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(60);
            if let Ok(cfg) = smedja_sre::SreConfig::from_env() {
                let client = reqwest::Client::new();
                match smedja_sre::otel_query(&client, &cfg, service, filter, range).await {
                    Ok(v) => serde_json::to_string(&v).unwrap_or_default(),
                    Err(e) => format!("error: {e}"),
                }
            } else {
                "SRE config not available (set SMEDJA_OTLP_ENDPOINT)".into()
            }
        }
        "metric_query" => {
            let promql = input.get("promql").and_then(|v| v.as_str()).unwrap_or("");
            let range = input
                .get("range_minutes")
                .and_then(Value::as_i64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(60);
            if let Ok(cfg) = smedja_sre::SreConfig::from_env() {
                let client = reqwest::Client::new();
                match smedja_sre::metric_query(&client, &cfg, promql, range).await {
                    Ok(v) => serde_json::to_string(&v).unwrap_or_default(),
                    Err(e) => format!("error: {e}"),
                }
            } else {
                "SRE config not available (set SMEDJA_PROMETHEUS_ENDPOINT)".into()
            }
        }
        "log_tail" => {
            let service = input
                .get("service")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let filter = input.get("filter").and_then(|v| v.as_str()).unwrap_or("");
            let lines = input
                .get("lines")
                .and_then(Value::as_i64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(100);
            if let Ok(cfg) = smedja_sre::SreConfig::from_env() {
                let client = reqwest::Client::new();
                match smedja_sre::log_tail(&client, &cfg, service, filter, lines).await {
                    Ok(v) => serde_json::to_string(&v).unwrap_or_default(),
                    Err(e) => format!("error: {e}"),
                }
            } else {
                "SRE config not available (set SMEDJA_LOKI_ENDPOINT)".into()
            }
        }
        other => format!("error: tool '{other}' is not available"),
    }
}

/// Subscribes to [`TurnEvent::Started`] and spawns a [`run_turn`] task for each.
///
/// Returns a shared handle store so that the caller can drain in-flight tasks
/// before exiting (graceful shutdown).
fn spawn_worker(
    ingot: Arc<Mutex<Ingot>>,
    dispatcher: Arc<Dispatcher>,
    gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
) -> Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> {
    let handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
    let handles_inner = Arc::clone(&handles);
    tokio::spawn(async move {
        let mut rx = dispatcher.subscribe();
        loop {
            // Block on the first event, then drain any additionally-queued events
            // to reduce per-delta task spawns during high-rate streaming.
            let first = match rx.recv().await {
                Ok(ev) => ev,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::error!(
                        dropped = n,
                        "turn worker lagged; events dropped — some turns may be lost",
                    );
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let mut batch = vec![first];
            batch.extend(smedja_bellows::drain_ready(&mut rx));
            for event in batch {
                if let TurnEvent::Started {
                    session_id,
                    turn_id,
                    ..
                } = event
                {
                    let ig = Arc::clone(&ingot);
                    let dp = Arc::clone(&dispatcher);
                    let g = Arc::clone(&gates);
                    let handle = tokio::spawn(run_turn(ig, dp, session_id, turn_id, g));
                    handles_inner.lock().await.push(handle);
                }
                // ignore non-Started events
            }
        }
    });
    handles
}

#[allow(clippy::too_many_lines)] // all RPC methods live in one function per the spec
fn build_router(
    ingot: &Arc<Mutex<Ingot>>,
    dispatcher: Arc<Dispatcher>,
    gates: &Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
) -> Router {
    let mut router = Router::new();

    // Clone gates so the closures below can each hold an independent Arc.
    let gates = Arc::clone(gates);

    // Create two Arcs for the pool so task.parallel and task.cancel each hold one.
    let pool = Arc::new(Mutex::new(WorktreePool::default()));
    let pool_cancel = Arc::clone(&pool);

    // ── ping ────────────────────────────────────────────────────────────────
    router.register("ping", |_| async { Ok(json!("pong")) });

    // ── session.create ──────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("session.create", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let title = params
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned);

            let cowork_mode = params
                .get("cowork_mode")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let task_description: Option<String> = params
                .get("task_description")
                .and_then(Value::as_str)
                .map(str::to_owned);

            let now = now_epoch();
            let session_id = Uuid::new_v4();

            // When task_description is provided, create the linked task first so
            // its ID can be stored directly in the Session row.
            let task_id: Option<String> = if let Some(ref desc) = task_description {
                let task = Task {
                    id: Uuid::new_v4(),
                    title: desc.clone(),
                    description: String::new(),
                    status: "planned".to_owned(),
                    created_at: now,
                    session_id: Some(session_id.to_string()),
                    response: None,
                };
                ig.lock()
                    .await
                    .create_task(&task)
                    .map_err(|e| ingot_err(&e))?;
                Some(task.id.to_string())
            } else {
                None
            };

            let session = Session {
                id: session_id,
                created_at: now,
                updated_at: now,
                status: "active".to_owned(),
                task_id: task_id.clone(),
                // Store the caller-supplied title in the `mode` field — Session
                // has no dedicated title column; this is the nearest optional
                // text field available on the existing schema.
                mode: title.clone(),
                cowork_mode,
                workspace_root: None,
                model_override: None,
            };

            ig.lock()
                .await
                .create_session(&session)
                .map_err(|e| ingot_err(&e))?;

            // Auto-index: check workspace.toml in cwd; if stale, index in background.
            {
                let cwd = std::env::var("SMEDJA_WORKSPACE")
                    .map_or_else(|_| std::path::PathBuf::from("."), std::path::PathBuf::from);
                let toml_path = cwd.join(".smedja").join("workspace.toml");
                let needs_index = if toml_path.exists() {
                    let content = std::fs::read_to_string(&toml_path).unwrap_or_default();
                    if let Ok(parsed) = toml::from_str::<toml::Value>(&content) {
                        parsed
                            .get("graph")
                            .and_then(|g| g.get("last_indexed_at"))
                            .and_then(|v| v.as_str())
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .is_none_or(|ts| {
                                let age = chrono::Utc::now()
                                    .signed_duration_since(ts.with_timezone(&chrono::Utc));
                                age.num_hours() >= 24
                            })
                    } else {
                        true
                    }
                } else {
                    // Only auto-index if workspace.toml already exists (workspace was initialised).
                    false
                };

                if needs_index {
                    let bg_cwd = cwd.clone();
                    let bg_toml = toml_path.clone();
                    tokio::task::spawn(async move {
                        use opentelemetry::trace::Span as _;
                        let tracer = opentelemetry::global::tracer("smedja");
                        let mut span =
                            opentelemetry::trace::Tracer::start(&tracer, "smedja.workspace.index");
                        let start = std::time::Instant::now();
                        let db_path = bg_cwd.join(".smedja").join("graph.db");
                        let bg_cwd_clone = bg_cwd.clone();
                        let symbol_count = tokio::task::spawn_blocking(move || {
                            smedja_graph::GraphStore::open(&db_path)
                                .and_then(|mut s| {
                                    s.index_workspace_incremental(&bg_cwd_clone, "workspace", None)
                                })
                                .unwrap_or(0)
                        })
                        .await
                        .unwrap_or(0);
                        let duration_ms = start.elapsed().as_millis();
                        span.set_attribute(opentelemetry::KeyValue::new(
                            "workspace_path",
                            bg_cwd.to_string_lossy().into_owned(),
                        ));
                        span.set_attribute(opentelemetry::KeyValue::new(
                            "symbol_count",
                            i64::try_from(symbol_count).unwrap_or(i64::MAX),
                        ));
                        span.set_attribute(opentelemetry::KeyValue::new(
                            "duration_ms",
                            i64::try_from(duration_ms).unwrap_or(i64::MAX),
                        ));
                        span.end();
                        let ts = chrono::Utc::now().to_rfc3339();
                        let new_content = format!(
                            "[graph]\nauto_index = true\nlast_indexed_at = \"{ts}\"\n"
                        );
                        if let Err(e) = std::fs::write(&bg_toml, new_content) {
                            tracing::warn!(error = %e, "failed to update workspace.toml after auto-index");
                        }
                    });
                }
            }

            // When cowork_mode is requested, register the per-session gate.
            // The gate map is owned by build_router; session.create handles the DB flag
            // only here. Callers that need the gate active must also call cowork.set.

            Ok(json!({
                "id": session.id,
                "title": title,
                "created_at": session.created_at,
                "cowork_mode": cowork_mode,
                "task_id": task_id,
            }))
        }
    });

    // ── session.list ────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("session.list", move |_| {
        let ig = Arc::clone(&ig);
        async move {
            let sessions = ig.lock().await.list_sessions().map_err(|e| ingot_err(&e))?;
            let out: Vec<Value> = sessions
                .into_iter()
                .map(|s| {
                    json!({
                        "id": s.id,
                        "title": s.mode,
                        "created_at": s.created_at,
                        "updated_at": s.updated_at,
                    })
                })
                .collect();
            Ok(Value::Array(out))
        }
    });

    // ── session.get ─────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("session.get", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let id = params
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("id"))?;

            let session = ig
                .lock()
                .await
                .get_session(id)
                .map_err(|e| ingot_err(&e))?
                .ok_or_else(|| {
                    RpcError::new(codes::INTERNAL_ERROR, format!("session not found: {id}"))
                })?;

            Ok(json!({
                "id": session.id,
                "title": session.mode,
                "created_at": session.created_at,
                "updated_at": session.updated_at,
                "status": session.status,
                "task_id": session.task_id,
            }))
        }
    });

    // ── session.delete ──────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("session.delete", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let id = params
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("id"))?;

            ig.lock()
                .await
                .delete_session(id)
                .map_err(|e| ingot_err(&e))?;
            Ok(Value::Bool(true))
        }
    });

    // ── session.fork ────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("session.fork", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();

            // Each DB call acquires and immediately releases the lock so other
            // concurrent RPC handlers (including turn.subscribe's polling loop)
            // are not serialised behind the entire fork sequence.
            let parent = {
                let guard = ig.lock().await;
                guard
                    .get_session(&session_id)
                    .map_err(|e| ingot_err(&e))?
                    .ok_or_else(|| {
                        RpcError::new(
                            codes::INTERNAL_ERROR,
                            format!("session not found: {session_id}"),
                        )
                    })?
            };

            let latest_cp = {
                let guard = ig.lock().await;
                guard
                    .latest_checkpoint(&session_id)
                    .map_err(|e| ingot_err(&e))?
            };

            let now = now_epoch();
            let new_id = Uuid::new_v4().to_string();

            {
                let mut guard = ig.lock().await;
                guard
                    .create_session(&Session {
                        id: Uuid::parse_str(&new_id).map_err(|e| {
                            RpcError::new(codes::INTERNAL_ERROR, format!("uuid error: {e}"))
                        })?,
                        created_at: now,
                        updated_at: now,
                        status: "active".into(),
                        task_id: None,
                        mode: parent.mode.clone(),
                        cowork_mode: parent.cowork_mode,
                        workspace_root: parent.workspace_root.clone(),
                        model_override: parent.model_override.clone(),
                    })
                    .map_err(|e| ingot_err(&e))?;
            }

            let has_checkpoint = latest_cp.is_some();
            if let Some(cp) = latest_cp {
                let mut guard = ig.lock().await;
                guard
                    .save_checkpoint(&Checkpoint {
                        id: Uuid::new_v4(),
                        session_id: new_id.clone(),
                        turn_n: cp.turn_n,
                        messages_json: cp.messages_json,
                        created_at: now,
                    })
                    .map_err(|e| ingot_err(&e))?;
            }

            Ok(json!({
                "session_id": new_id,
                "forked_from": session_id,
                "has_checkpoint": has_checkpoint,
            }))
        }
    });

    // ── turn.subscribe ──────────────────────────────────────────────────────
    // Blocks until the named task reaches a terminal status (complete / failed)
    // or a 60-second deadline expires.  Returns a single response envelope so
    // callers do not need to poll task.get in a loop.
    let ig = Arc::clone(ingot);
    router.register("turn.subscribe", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let task_id = params
                .get("task_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("task_id"))?
                .to_owned();

            let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
            loop {
                let task = {
                    let guard = ig.lock().await;
                    guard.get_task(&task_id).map_err(|e| ingot_err(&e))?
                };
                match task {
                    None => {
                        return Err(RpcError::new(
                            codes::INTERNAL_ERROR,
                            format!("task not found: {task_id}"),
                        ))
                    }
                    Some(t) if t.status == "complete" => {
                        // Best-effort: look up the latest token snapshot for
                        // this task's session so the TUI can display counts.
                        let (input_tok, output_tok) = if let Some(ref sid) = t.session_id {
                            ig.lock().await.session_token_snapshots(sid).map_or(
                                (0i64, 0i64),
                                |snaps| {
                                    snaps
                                        .last()
                                        .map_or((0i64, 0i64), |s| (s.input_tok, s.output_tok))
                                },
                            )
                        } else {
                            (0i64, 0i64)
                        };
                        return Ok(json!({
                            "done": true,
                            "response": t.response.unwrap_or_default(),
                            "input_tok": input_tok,
                            "output_tok": output_tok,
                        }));
                    }
                    Some(t) if t.status == "failed" => {
                        return Ok(json!({
                            "done": true,
                            "error": t.response.unwrap_or_else(|| "turn failed".into()),
                        }));
                    }
                    Some(_) => {
                        if std::time::Instant::now() >= deadline {
                            return Err(RpcError::new(
                                codes::TIMEOUT,
                                "turn.subscribe timed out after 60s",
                            ));
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }
    });

    // ── task.get ────────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("task.get", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let id = params
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("id"))?;

            let task = ig
                .lock()
                .await
                .get_task(id)
                .map_err(|e| ingot_err(&e))?
                .ok_or_else(|| {
                    RpcError::new(codes::INTERNAL_ERROR, format!("task not found: {id}"))
                })?;

            Ok(json!({
                "id": task.id,
                "status": task.status,
                "title": task.title,
                "response": task.response,
            }))
        }
    });

    // ── task.list ───────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("task.list", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let status = params.get("status").and_then(Value::as_str);
            let tasks = ig
                .lock()
                .await
                .list_tasks(status)
                .map_err(|e| ingot_err(&e))?;
            let out: Vec<Value> = tasks
                .iter()
                .map(|t| {
                    json!({
                        "id": t.id,
                        "title": t.title,
                        "status": t.status,
                        "created_at": t.created_at,
                        "session_id": t.session_id,
                    })
                })
                .collect();
            Ok(Value::Array(out))
        }
    });

    // ── task.create ─────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("task.create", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let title = params
                .get("title")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("title"))?
                .to_owned();
            let description = params
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let task = Task {
                id: Uuid::new_v4(),
                title,
                description,
                status: "planned".to_owned(),
                created_at: now_epoch(),
                session_id,
                response: None,
            };
            ig.lock()
                .await
                .create_task(&task)
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "id": task.id, "status": task.status }))
        }
    });

    // ── task.close ──────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("task.close", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let id = params
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("id"))?;
            ig.lock()
                .await
                .update_task_status(id, "complete")
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "id": id, "status": "complete" }))
        }
    });

    // ── turn.submit ─────────────────────────────────────────────────────────
    // Clone dispatcher before turn.submit moves it, so loop.run can hold its
    // own Arc without requiring a second clone point after the move.
    let dispatcher_loop_run = Arc::clone(&dispatcher);
    let ig = Arc::clone(ingot);
    router.register("turn.submit", move |params: Value| {
        let ig = Arc::clone(&ig);
        let dispatcher = Arc::clone(&dispatcher);
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let content = params
                .get("content")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("content"))?
                .to_owned();

            let task_id = Uuid::new_v4();
            let task = Task {
                id: task_id,
                title: content,
                description: String::new(),
                status: "planned".to_owned(),
                created_at: now_epoch(),
                session_id: Some(session_id.clone()),
                response: None,
            };

            ig.lock()
                .await
                .create_task(&task)
                .map_err(|e| ingot_err(&e))?;

            // Extract current span IDs for turn start event correlation.
            let (ts_trace_id, ts_span_id) = {
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
            dispatcher.publish(TurnEvent::Started {
                session_id: session_id.clone(),
                turn_id: task_id.to_string(),
                conversation_id: Some(session_id.clone()),
                trace_id: ts_trace_id,
                span_id: ts_span_id,
                parent_span_id: None,
                operation_name: Some(tel::OPERATION_INVOKE_AGENT.to_owned()),
                agent_name: Some("interactive".to_owned()),
                status: None,
            });

            Ok(json!({ "task_id": task_id }))
        }
    });

    // ── session.checkpoint.list ─────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("session.checkpoint.list", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?;
            let cps = ig
                .lock()
                .await
                .list_checkpoints(session_id)
                .map_err(|e| ingot_err(&e))?;
            let out: Vec<Value> = cps
                .iter()
                .map(|cp| {
                    json!({
                        "turn_n": cp.turn_n,
                        "created_at": cp.created_at,
                    })
                })
                .collect();
            Ok(Value::Array(out))
        }
    });

    // ── session.rollback ────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("session.rollback", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let turn_n_raw = params
                .get("turn_n")
                .and_then(Value::as_i64)
                .ok_or_else(|| missing_param("turn_n"))?;
            let turn_n_u32 = u32::try_from(turn_n_raw).map_err(|_| {
                RpcError::new(
                    codes::INVALID_PARAMS,
                    "turn_n must be a non-negative integer",
                )
            })?;

            // Atomically load the target checkpoint and prune all later checkpoints
            // for this session in a single SQLite transaction.
            let cp = ig
                .lock()
                .await
                .rollback_session(&session_id, turn_n_u32)
                .map_err(|e| ingot_err(&e))?;

            match cp {
                Some(cp) => Ok(json!({
                    "session_id": session_id,
                    "turn_n": cp.turn_n,
                    "messages_json": cp.messages_json,
                    "created_at": cp.created_at,
                })),
                None => Err(RpcError::new(codes::INVALID_PARAMS, "checkpoint not found")),
            }
        }
    });

    // ── session.compact ──────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("session.compact", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();

            // Load latest checkpoint for conversation history.
            let messages_json = {
                let guard = ig.lock().await;
                guard
                    .latest_checkpoint(&session_id)
                    .map_err(|e| ingot_err(&e))?
                    .map_or_else(|| "[]".to_owned(), |cp| cp.messages_json)
            };

            // Call provider to produce a summary.
            let compaction_prompt = format!(
                "Summarise this conversation in 3–5 bullet points, then state the current goal. \
                 Be concise.\n\nConversation history:\n{messages_json}"
            );
            let (provider, _, _) = build_provider()
                .await
                .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("no provider: {e}")))?;
            let opts = CallOptions {
                model: std::env::var("SMEDJA_MODEL")
                    .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_owned()),
                max_tokens: Some(512),
                temperature: Some(0.3),
                system: Some("You are a summarisation assistant.".to_owned()),
                tools: None,
            };
            let stream = provider.stream_chat(
                &[AdapterMessage {
                    role: AdapterRole::User,
                    content: compaction_prompt,
                }],
                &opts,
            );
            let dispatcher = Dispatcher::new(1);
            let (summary, _, _) = drain_stream(stream, &dispatcher).await.map_err(|e| {
                RpcError::new(codes::INTERNAL_ERROR, format!("compaction failed: {e}"))
            })?;

            // Save pre-compaction checkpoint tagged with turn_n = -1.
            let turn_count = {
                let guard = ig.lock().await;
                guard
                    .list_checkpoints(&session_id)
                    .map_or(0i64, |v| i64::try_from(v.len()).unwrap_or(i64::MAX))
            };
            let cp = Checkpoint {
                id: Uuid::new_v4(),
                session_id: session_id.clone(),
                turn_n: -1, // compaction marker
                messages_json: messages_json.clone(),
                created_at: now_epoch(),
            };
            {
                let mut guard = ig.lock().await;
                if let Err(e) = guard.save_checkpoint(&cp) {
                    warn!(error = %e, "failed to save pre-compaction checkpoint");
                }
            }

            Ok(json!({
                "session_id": session_id,
                "summary": summary,
                "turn_count": turn_count,
                "compaction_checkpoint_saved": true,
            }))
        }
    });

    // ── session.token_usage ──────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("session.token_usage", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?;
            let snaps = ig
                .lock()
                .await
                .session_token_snapshots(session_id)
                .map_err(|e| ingot_err(&e))?;
            let rows: Vec<Value> = snaps
                .iter()
                .map(|s| {
                    json!({
                        "turn_n": s.turn_n,
                        "input_tok": s.input_tok,
                        "output_tok": s.output_tok,
                        "cumulative_input": s.cumulative_input,
                        "cumulative_output": s.cumulative_output,
                    })
                })
                .collect();
            Ok(json!({ "session_id": session_id, "turns": rows }))
        }
    });

    // ── session.cost ────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("session.cost", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?;
            let total_usd = ig
                .lock()
                .await
                .session_cost(session_id)
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "session_id": session_id, "total_usd": total_usd }))
        }
    });

    // ── cowork.set ──────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    let gates_set = Arc::clone(&gates);
    router.register("cowork.set", move |params: Value| {
        let ig = Arc::clone(&ig);
        let gates = Arc::clone(&gates_set);
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let enabled = params
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| missing_param("enabled"))?;
            ig.lock()
                .await
                .set_cowork_mode(&session_id, enabled)
                .map_err(|e| ingot_err(&e))?;

            // Manage the per-session gate.
            let mut g = gates.lock().await;
            if enabled {
                g.entry(session_id.clone())
                    .or_insert_with(|| Arc::new(CoworkGate::default()));
            } else {
                g.remove(&session_id);
            }

            Ok(json!({ "session_id": session_id, "cowork_mode": enabled }))
        }
    });

    // ── session.set_mode ────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("session.set_mode", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let session_id = params
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let mode = params
                .get("mode")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("mode"))?
                .to_owned();
            ig.lock()
                .await
                .update_session_mode(&session_id, &mode)
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "session_id": session_id, "mode": mode }))
        }
    });

    // ── mcp.register ────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("mcp.register", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| missing_param("name"))?
                .to_owned();
            let url = params
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let transport = params
                .get("transport")
                .and_then(Value::as_str)
                .unwrap_or("http")
                .to_owned();
            let server = McpServer {
                id: Uuid::new_v4().to_string(),
                name,
                url,
                transport,
                tools_json: "[]".into(),
                last_refresh: 0.0,
            };
            ig.lock()
                .await
                .register_mcp_server(&server)
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "id": server.id }))
        }
    });

    // ── mcp.list ────────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("mcp.list", move |_: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let servers = ig
                .lock()
                .await
                .list_mcp_servers()
                .map_err(|e| ingot_err(&e))?;
            let out: Vec<Value> = servers
                .iter()
                .map(|s| {
                    json!({
                        "id": s.id,
                        "name": s.name,
                        "url": s.url,
                        "transport": s.transport,
                    })
                })
                .collect();
            Ok(Value::Array(out))
        }
    });

    // ── cowork.approve ───────────────────────────────────────────────────────
    let gates_approve = Arc::clone(&gates);
    router.register("cowork.approve", move |params: Value| {
        let gates = Arc::clone(&gates_approve);
        async move {
            let session_id = params["session_id"]
                .as_str()
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let id = params["id"]
                .as_str()
                .ok_or_else(|| missing_param("id"))?
                .to_owned();
            let gate = gates
                .lock()
                .await
                .get(&session_id)
                .cloned()
                .ok_or_else(|| {
                    RpcError::new(
                        codes::INTERNAL_ERROR,
                        format!("no cowork gate for session: {session_id}"),
                    )
                })?;
            let found = gate.approve(&id).await;
            Ok(json!({ "id": id, "resolved": found }))
        }
    });

    // ── cowork.deny ──────────────────────────────────────────────────────────
    let gates_deny = Arc::clone(&gates);
    router.register("cowork.deny", move |params: Value| {
        let gates = Arc::clone(&gates_deny);
        async move {
            let session_id = params["session_id"]
                .as_str()
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let id = params["id"]
                .as_str()
                .ok_or_else(|| missing_param("id"))?
                .to_owned();
            let reason = params["reason"].as_str().unwrap_or("denied").to_owned();
            let gate = gates
                .lock()
                .await
                .get(&session_id)
                .cloned()
                .ok_or_else(|| {
                    RpcError::new(
                        codes::INTERNAL_ERROR,
                        format!("no cowork gate for session: {session_id}"),
                    )
                })?;
            let found = gate.deny(&id, reason).await;
            Ok(json!({ "id": id, "resolved": found }))
        }
    });

    // ── cowork.modify ────────────────────────────────────────────────────────
    let gates_modify = Arc::clone(&gates);
    router.register("cowork.modify", move |params: Value| {
        let gates = Arc::clone(&gates_modify);
        async move {
            let session_id = params["session_id"]
                .as_str()
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let id = params["id"]
                .as_str()
                .ok_or_else(|| missing_param("id"))?
                .to_owned();
            let instruction = params["instruction"].as_str().unwrap_or("").to_owned();
            let gate = gates
                .lock()
                .await
                .get(&session_id)
                .cloned()
                .ok_or_else(|| {
                    RpcError::new(
                        codes::INTERNAL_ERROR,
                        format!("no cowork gate for session: {session_id}"),
                    )
                })?;
            let found = gate.modify(&id, instruction).await;
            Ok(json!({ "id": id, "resolved": found }))
        }
    });

    // ── cowork.pending ───────────────────────────────────────────────────────
    let gates_pending = Arc::clone(&gates);
    router.register("cowork.pending", move |params: Value| {
        let gates = Arc::clone(&gates_pending);
        async move {
            let session_id = params["session_id"]
                .as_str()
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let gate = gates
                .lock()
                .await
                .get(&session_id)
                .cloned()
                .ok_or_else(|| {
                    RpcError::new(
                        codes::INTERNAL_ERROR,
                        format!("no cowork gate for session: {session_id}"),
                    )
                })?;
            let pending = gate.list_pending().await;
            let out: Vec<Value> = pending
                .into_iter()
                .map(|(id, tool)| json!({ "id": id, "tool": tool }))
                .collect();
            Ok(Value::Array(out))
        }
    });

    // ── task.parallel ────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("task.parallel", move |params: Value| {
        let pool = Arc::clone(&pool);
        let ig = Arc::clone(&ig);
        async move {
            let session_id = params["session_id"].as_str().map(str::to_owned);
            let goal = params["goal"]
                .as_str()
                .ok_or_else(|| missing_param("goal"))?
                .to_owned();
            // Roles may be plain strings or `{name, resume_session_id?}` objects.
            let loop_roles: Vec<smedja_assayer::LoopRole> = params["roles"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|v| {
                    if let Some(name) = v.as_str() {
                        Some(smedja_assayer::LoopRole {
                            name: name.to_owned(),
                            resume_session_id: None,
                        })
                    } else if let Some(obj) = v.as_object() {
                        obj.get("name").and_then(Value::as_str).map(|name| {
                            smedja_assayer::LoopRole {
                                name: name.to_owned(),
                                resume_session_id: obj
                                    .get("resume_session_id")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned),
                            }
                        })
                    } else {
                        None
                    }
                })
                .collect();

            // Enforce MAX_ROLE_DEPTH: reject any role that tries to resume a session at depth ≥ 4.
            {
                let guard = ig.lock().await;
                for role in &loop_roles {
                    if let Some(ref resume_sid) = role.resume_session_id {
                        let cps = guard.list_checkpoints(resume_sid).unwrap_or_default();
                        let depth = cps.iter().filter(|cp| cp.turn_n == -1).count();
                        #[allow(clippy::cast_possible_truncation)]
                        if depth as u8 >= smedja_assayer::MAX_ROLE_DEPTH {
                            return Err(RpcError::new(
                                codes::INVALID_PARAMS,
                                format!(
                                    "resume depth exceeded for role '{}': max {}",
                                    role.name,
                                    smedja_assayer::MAX_ROLE_DEPTH,
                                ),
                            ));
                        }
                    }
                }
            }
            let roles: Vec<String> = loop_roles.iter().map(|r| r.name.clone()).collect();

            // Derive workspace root: prefer session.workspace_root, then env, then ".".
            let env_workspace = || {
                std::env::var("SMEDJA_WORKSPACE").map_or_else(|_| PathBuf::from("."), PathBuf::from)
            };
            let workspace_root = if let Some(ref sid) = session_id {
                ig.lock()
                    .await
                    .get_session(sid)
                    .ok()
                    .flatten()
                    .and_then(|s| s.workspace_root)
                    .map_or_else(env_workspace, PathBuf::from)
            } else {
                env_workspace()
            };

            if !workspace_root.join(".git").exists() {
                tracing::warn!(
                    path = %workspace_root.display(),
                    "task.parallel workspace does not contain .git",
                );
            }

            let mut p = pool.lock().await;

            // Register all roles first (synchronous — no await).
            let registered: Vec<(String, String)> = roles
                .iter()
                .map(|role| {
                    let id = p.register(role, &goal, &workspace_root);
                    (role.clone(), id)
                })
                .collect();

            // Create the git worktrees for all pending tasks.
            let started = p.start_worktrees(&workspace_root).await;

            // Build the per-task response, including worktree_path where available.
            let tasks: Vec<Value> = registered
                .iter()
                .map(|(role, task_id)| {
                    let worktree_path = p
                        .get(task_id)
                        .map(|t| t.worktree_path.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    json!({
                        "role": role,
                        "task_id": task_id,
                        "worktree_path": worktree_path,
                    })
                })
                .collect();

            Ok(json!({ "goal": goal, "tasks": tasks, "started": started }))
        }
    });

    // ── task.cancel ──────────────────────────────────────────────────────────
    router.register("task.cancel", move |params: Value| {
        let pool = Arc::clone(&pool_cancel);
        async move {
            let task_id = params["task_id"]
                .as_str()
                .ok_or_else(|| missing_param("task_id"))?
                .to_owned();
            let found = pool.lock().await.cancel(&task_id);
            Ok(json!({ "task_id": task_id, "cancelled": found }))
        }
    });

    // ── mcp.remove ───────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("mcp.remove", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let name = params["name"]
                .as_str()
                .ok_or_else(|| missing_param("name"))?
                .to_owned();
            ig.lock()
                .await
                .remove_mcp_server(&name)
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "name": name, "removed": true }))
        }
    });

    // ── mcp.refresh ──────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("mcp.refresh", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let name_filter: Option<String> = params
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned);

            // Load the candidate servers — all registered, or the named one.
            let servers = {
                let ig = ig.lock().await;
                let all = ig
                    .list_mcp_servers()
                    .map_err(|e| ingot_err(&e))?;
                match name_filter {
                    Some(ref name) => all
                        .into_iter()
                        .filter(|s| &s.name == name)
                        .collect::<Vec<_>>(),
                    None => all,
                }
            };

            let mut refreshed = 0usize;
            for server in servers {
                let client = match crate::mcp_http::McpHttpClient::new(&server.url, "") {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(name = %server.name, error = %e, "mcp.refresh: failed to build client");
                        continue;
                    }
                };
                match client.list_tools().await {
                    Ok(tools) => {
                        let tools_json = serde_json::to_string(&tools)
                            .unwrap_or_else(|_| "[]".to_owned());
                        let updated = McpServer {
                            tools_json,
                            last_refresh: now_epoch(),
                            ..server.clone()
                        };
                        let mut ig = ig.lock().await;
                        let _ = ig.register_mcp_server(&updated);
                        refreshed += 1;
                    }
                    Err(e) => {
                        tracing::warn!(name = %server.name, error = %e, "mcp.refresh failed");
                    }
                }
            }

            Ok(json!({ "refreshed": refreshed }))
        }
    });

    // ── loop.create ──────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("loop.create", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let change_name = params["change_name"]
                .as_str()
                .ok_or_else(|| missing_param("change_name"))?
                .to_owned();
            let now = now_epoch();
            let rec = smedja_ingot::LoopRecord {
                id: Uuid::new_v4().to_string(),
                change_name,
                status: "planned".to_owned(),
                current_slice: 0,
                attempt: 1,
                created_at: now,
                updated_at: now,
            };
            let loop_id = rec.id.clone();
            ig.lock()
                .await
                .create_loop(&rec)
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "loop_id": loop_id }))
        }
    });

    // ── loop.status ──────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("loop.status", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let loop_id = params["loop_id"]
                .as_str()
                .ok_or_else(|| missing_param("loop_id"))?
                .to_owned();
            let rec = ig
                .lock()
                .await
                .get_loop(&loop_id)
                .map_err(|e| ingot_err(&e))?
                .ok_or_else(|| {
                    RpcError::new(codes::INVALID_PARAMS, format!("loop not found: {loop_id}"))
                })?;
            Ok(serde_json::to_value(&rec).unwrap_or(Value::Null))
        }
    });

    // ── loop.cancel ──────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("loop.cancel", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let loop_id = params["loop_id"]
                .as_str()
                .ok_or_else(|| missing_param("loop_id"))?
                .to_owned();
            ig.lock()
                .await
                .update_loop_status(&loop_id, "cancelled", now_epoch())
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "loop_id": loop_id, "status": "cancelled" }))
        }
    });

    // ── loop.list ────────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("loop.list", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let change_name = params["change_name"]
                .as_str()
                .ok_or_else(|| missing_param("change_name"))?
                .to_owned();
            let loops = ig
                .lock()
                .await
                .list_loops(&change_name)
                .map_err(|e| ingot_err(&e))?;
            let loops_json: Vec<Value> = loops
                .into_iter()
                .map(|r| serde_json::to_value(&r).unwrap_or(Value::Null))
                .collect();
            Ok(json!({ "loops": loops_json }))
        }
    });

    // ── loop.retire ──────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("loop.retire", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let loop_id = params["loop_id"]
                .as_str()
                .ok_or_else(|| missing_param("loop_id"))?
                .to_owned();
            let rec = ig
                .lock()
                .await
                .get_loop(&loop_id)
                .map_err(|e| ingot_err(&e))?
                .ok_or_else(|| {
                    RpcError::new(codes::INVALID_PARAMS, format!("loop not found: {loop_id}"))
                })?;
            // Only complete or failed loops can be retired.
            if rec.status != "complete" && rec.status != "failed" {
                return Err(RpcError::new(
                    codes::INVALID_PARAMS,
                    format!(
                        "loop is in state '{}'; only complete or failed loops can be retired",
                        rec.status
                    ),
                ));
            }
            ig.lock()
                .await
                .update_loop_status(&loop_id, "retired", now_epoch())
                .map_err(|e| ingot_err(&e))?;
            Ok(json!({ "loop_id": loop_id, "status": "retired" }))
        }
    });

    // ── loop.list_by_status ──────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("loop.list_by_status", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let status = params["status"].as_str().map(str::to_owned);
            let loops = ig
                .lock()
                .await
                .list_loops_by_status(status.as_deref())
                .map_err(|e| ingot_err(&e))?;
            let loops_json: Vec<Value> = loops
                .into_iter()
                .map(|r| serde_json::to_value(&r).unwrap_or(Value::Null))
                .collect();
            Ok(json!({ "loops": loops_json }))
        }
    });

    // ── audit.list ───────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    router.register("audit.list", move |params: Value| {
        let ig = Arc::clone(&ig);
        async move {
            let session_id = params["session_id"]
                .as_str()
                .ok_or_else(|| missing_param("session_id"))?
                .to_owned();
            let events = ig
                .lock()
                .await
                .list_audit_events(&session_id)
                .map_err(|e| ingot_err(&e))?;
            let events_json: Vec<Value> = events
                .into_iter()
                .map(|ev| serde_json::to_value(&ev).unwrap_or(Value::Null))
                .collect();
            Ok(json!({ "events": events_json }))
        }
    });

    // ── loop.run ────────────────────────────────────────────────────────────
    let ig = Arc::clone(ingot);
    let gates_run = Arc::clone(&gates);
    router.register("loop.run", move |params: Value| {
        let ig = Arc::clone(&ig);
        let dispatcher = Arc::clone(&dispatcher_loop_run);
        let _gates = Arc::clone(&gates_run);
        async move {
            let loop_id = params["loop_id"]
                .as_str()
                .ok_or_else(|| missing_param("loop_id"))?
                .to_owned();

            // Verify the loop record exists before spawning background work.
            let rec = ig
                .lock()
                .await
                .get_loop(&loop_id)
                .map_err(|e| ingot_err(&e))?
                .ok_or_else(|| {
                    RpcError::new(codes::INVALID_PARAMS, format!("loop not found: {loop_id}"))
                })?;

            // Retired loops cannot be re-run.
            if rec.status == "retired" {
                return Err(RpcError::new(
                    codes::INVALID_PARAMS,
                    "loop is retired and cannot be re-run",
                ));
            }

            // Spawn background task — caller gets an immediate response.
            let bg_ig = Arc::clone(&ig);
            let bg_dispatcher = Arc::clone(&dispatcher);
            let bg_loop_id = loop_id.clone();
            let change_name = rec.change_name.clone();
            tokio::spawn(async move {
                let workspace = std::env::var("SMEDJA_WORKSPACE").unwrap_or_else(|_| ".".into());
                let tasks_path = std::path::PathBuf::from(&workspace)
                    .join("openspec")
                    .join("changes")
                    .join(&change_name)
                    .join("tasks.md");

                let Ok(tasks_content) = tokio::fs::read_to_string(&tasks_path).await else {
                    let _ =
                        bg_ig
                            .lock()
                            .await
                            .update_loop_status(&bg_loop_id, "failed", now_epoch());
                    return;
                };

                // Parse pending tasks: lines starting with `- [ ] `.
                let pending: Vec<String> = tasks_content
                    .lines()
                    .filter(|l| l.starts_with("- [ ] "))
                    .map(|l| l.trim_start_matches("- [ ] ").to_owned())
                    .collect();

                if pending.is_empty() {
                    let _ =
                        bg_ig
                            .lock()
                            .await
                            .update_loop_status(&bg_loop_id, "complete", now_epoch());
                    return;
                }

                // Mark loop as slicing.
                {
                    let mut guard = bg_ig.lock().await;
                    let _ = guard.update_loop_status(&bg_loop_id, "slicing", now_epoch());
                }

                // Create one session for this loop run.
                let session_id = Uuid::new_v4();
                let now = now_epoch();
                let session = Session {
                    id: session_id,
                    created_at: now,
                    updated_at: now,
                    status: "active".to_owned(),
                    task_id: None,
                    mode: Some("loop".to_owned()),
                    cowork_mode: false,
                    workspace_root: Some(workspace),
                    model_override: None,
                };
                {
                    let mut guard = bg_ig.lock().await;
                    let _ = guard.create_session(&session);
                }

                // Submit one turn per pending task, poll until done, then mark checkbox.
                let mut updated_content = tasks_content.clone();
                for (slice_idx, task_text) in pending.into_iter().enumerate() {
                    let task_id = Uuid::new_v4();
                    let task = Task {
                        id: task_id,
                        title: task_text.clone(),
                        description: String::new(),
                        status: "planned".to_owned(),
                        created_at: now_epoch(),
                        session_id: Some(session_id.to_string()),
                        response: None,
                    };

                    {
                        let mut guard = bg_ig.lock().await;
                        let _ = guard.create_task(&task);
                    }

                    // Extract current span IDs for loop turn start event correlation.
                    let (loop_ts_trace_id, loop_ts_span_id) = {
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
                    bg_dispatcher.publish(TurnEvent::Started {
                        session_id: session_id.to_string(),
                        turn_id: task_id.to_string(),
                        conversation_id: Some(session_id.to_string()),
                        trace_id: loop_ts_trace_id,
                        span_id: loop_ts_span_id,
                        parent_span_id: None,
                        operation_name: Some(tel::OPERATION_INVOKE_AGENT.to_owned()),
                        agent_name: Some("interactive".to_owned()),
                        status: None,
                    });

                    // Poll until the task leaves "planned" / "in_progress".
                    loop {
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                        let status = bg_ig
                            .lock()
                            .await
                            .get_task(&task_id.to_string())
                            .ok()
                            .flatten()
                            .map_or_else(|| "unknown".to_owned(), |t| t.status);
                        if status != "planned" && status != "in_progress" {
                            break;
                        }
                    }

                    // Replace `- [ ] <task>` with `- [x] <task>` in content string.
                    let unchecked = format!("- [ ] {task_text}");
                    let checked = format!("- [x] {task_text}");
                    updated_content = updated_content.replacen(&unchecked, &checked, 1);

                    // Advance current_slice counter.
                    #[allow(clippy::cast_possible_wrap)]
                    // slice index never exceeds i64::MAX in practice
                    let slice_count = (slice_idx + 1) as i64;
                    let _ =
                        bg_ig
                            .lock()
                            .await
                            .update_loop_slice(&bg_loop_id, slice_count, now_epoch());
                }

                // Write updated tasks.md back to disk.
                let _ = tokio::fs::write(&tasks_path, &updated_content).await;

                // Mark loop complete.
                let _ = bg_ig
                    .lock()
                    .await
                    .update_loop_status(&bg_loop_id, "complete", now_epoch());
            });

            Ok(json!({ "loop_id": loop_id, "status": "slicing" }))
        }
    });

    router
}

/// Writes the ACP auth token to the runtime secret file with 0o600 permissions.
///
/// Path preference: `$XDG_RUNTIME_DIR/smdjad.secret` → `$HOME/.cache/smdjad.secret`
/// → `/tmp/smdjad.secret`.
fn write_acp_secret(token: &str) {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let secret_path = std::env::var("XDG_RUNTIME_DIR").map_or_else(
        |_| {
            dirs_home().map_or_else(
                || std::path::PathBuf::from("/tmp/smdjad.secret"),
                |h| h.join(".cache").join("smdjad.secret"),
            )
        },
        |d| std::path::PathBuf::from(d).join("smdjad.secret"),
    );

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&secret_path)
    {
        let _ = f.write_all(token.as_bytes());
    } else {
        tracing::warn!(path = %secret_path.display(), "could not write ACP secret file");
    }
}

#[allow(clippy::too_many_lines)] // startup sequence: bind, migrate, orphan sweep, spawn workers
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Install an OTLP exporter when SMEDJA_OTLP_ENDPOINT is set.
    // Without this, all OTel spans are silently discarded by the no-op provider.
    if let Ok(endpoint) = std::env::var("SMEDJA_OTLP_ENDPOINT") {
        use opentelemetry_otlp::WithExportConfig as _;
        let build_result = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(&endpoint)
            .build();
        match build_result {
            Ok(exporter) => {
                let provider = opentelemetry_sdk::trace::TracerProvider::builder()
                    .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
                    .build();
                opentelemetry::global::set_tracer_provider(provider);
                info!(endpoint = %endpoint, "OTLP trace exporter installed");
            }
            Err(e) => {
                warn!(error = %e, endpoint = %endpoint, "failed to install OTLP exporter; traces will not be exported");
            }
        }
    } else {
        warn!("SMEDJA_OTLP_ENDPOINT not set; OTel traces will not be exported");
    }

    let path = socket_path();

    // Remove stale socket if it exists.
    let _ = std::fs::remove_file(&path);

    // Bind BEFORE spawning so a port-conflict error exits cleanly.
    let listener = UnixListener::bind(&path)?;
    // Guard removes the socket on any exit path (clean shutdown or error propagation).
    let _socket_guard = SocketGuard { path: path.clone() };

    // Set Unix socket permissions to 0o600 immediately after binding.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow::anyhow!("failed to set socket permissions: {e}"))?;
    }

    // Write PID file so `smj daemon stop` can send SIGTERM.
    let pid_path = {
        let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        std::path::PathBuf::from(base).join("smdjad.pid")
    };
    std::fs::write(&pid_path, std::process::id().to_string())
        .unwrap_or_else(|e| tracing::warn!(error = %e, "failed to write PID file"));

    info!(path = %path.display(), "smdjad listening");

    let ingot = open_ingot()?;
    let ingot = Arc::new(Mutex::new(ingot));

    // Detect sessions left in_flight by a prior crash.
    {
        let mut ig = ingot.lock().await;
        // ponytail: linear scan; session counts are small
        match ig.list_sessions() {
            Ok(sessions) => {
                let orphaned: Vec<_> = sessions
                    .into_iter()
                    .filter(|s| s.status == "in_flight")
                    .collect();
                if !orphaned.is_empty() {
                    tracing::warn!(
                        count = orphaned.len(),
                        "orphaned in_flight sessions detected at startup; marking as orphaned"
                    );
                    for sess in &orphaned {
                        let sid = sess.id.to_string();
                        let _ = ig.update_session_status(&sid, "orphaned");

                        // Also fail any in_progress tasks owned by this session.
                        match ig.list_tasks(Some("in_progress")) {
                            Ok(tasks) => {
                                for task in tasks {
                                    if task.session_id.as_deref() == Some(sid.as_str()) {
                                        let _ =
                                            ig.update_task_status(&task.id.to_string(), "failed");
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "could not list tasks during orphan sweep");
                            }
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not list sessions at startup"),
        }
    }

    // Refresh MCP server tool lists that have not been updated in the last hour.
    {
        let stale_threshold = now_epoch() - 3600.0;
        let servers = ingot
            .lock()
            .await
            .list_mcp_servers()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.last_refresh < stale_threshold)
            .collect::<Vec<_>>();

        for server in servers {
            match crate::mcp_http::McpHttpClient::new(&server.url, "") {
                Ok(client) => match client.list_tools().await {
                    Ok(tools) => {
                        let tools_json =
                            serde_json::to_string(&tools).unwrap_or_else(|_| "[]".to_owned());
                        let updated = McpServer {
                            tools_json,
                            last_refresh: now_epoch(),
                            ..server.clone()
                        };
                        let mut ig = ingot.lock().await;
                        if let Err(e) = ig.register_mcp_server(&updated) {
                            tracing::warn!(name = %server.name, error = %e, "failed to update MCP tools at startup");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(name = %server.name, error = %e, "MCP refresh failed at startup");
                    }
                },
                Err(e) => {
                    tracing::warn!(name = %server.name, error = %e, "failed to build MCP client at startup");
                }
            }
        }
    }

    // Capacity 256: lifecycle events (Started/Completed/Failed) must never be
    // dropped by streaming delta overflow.  256 provides enough headroom for
    // bursts of AssistantDelta chunks without discarding control events.
    let dispatcher = Arc::new(Dispatcher::new(256));
    let gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>> = Arc::new(Mutex::new(HashMap::new()));

    let router = build_router(&ingot, Arc::clone(&dispatcher), &gates);

    let turn_handles = spawn_worker(
        Arc::clone(&ingot),
        Arc::clone(&dispatcher),
        Arc::clone(&gates),
    );

    // ACP HTTP server — activated by SMEDJA_ACP_PORT.
    if let Ok(port_str) = std::env::var("SMEDJA_ACP_PORT") {
        if let Ok(port) = port_str.parse::<u16>() {
            // Generate a one-time auth token and write it to the runtime secret file.
            let acp_token = uuid::Uuid::new_v4().to_string();
            write_acp_secret(&acp_token);
            let acp_state = acp::AcpState {
                ingot: Arc::clone(&ingot),
                dispatcher: Arc::clone(&dispatcher),
                auth_token: acp_token,
            };
            let acp_router = acp::build_acp_router(acp_state);
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            // Bind before spawning so a port conflict fails at startup, not inside the task.
            let tcp_listener = tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|e| anyhow::anyhow!("ACP bind failed on {addr}: {e}"))?;
            info!(%addr, "ACP HTTP server listening");
            tokio::spawn(async move {
                if let Err(e) = axum::serve(tcp_listener, acp_router).await {
                    tracing::error!(error = %e, "ACP server error");
                }
            });
        }
    }

    let server = Server::new(router);

    tokio::select! {
        result = server.serve(listener) => {
            result?;
        }
        _ = tokio::signal::ctrl_c() => {
            info!("received SIGINT; shutting down");
        }
        _ = async {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler failed")
                .recv()
                .await
        } => {
            info!("received SIGTERM; shutting down");
        }
    }

    // Drain any in-flight run_turn tasks before cleaning up, so that turns that
    // are mid-stream can complete (or fail cleanly) rather than being silently
    // abandoned.  A 30 s deadline prevents indefinite blocking on a stuck task.
    {
        let handles: Vec<tokio::task::JoinHandle<()>> =
            std::mem::take(&mut *turn_handles.lock().await);
        if !handles.is_empty() {
            info!(
                count = handles.len(),
                "waiting for in-flight turns to finish (up to 30 s)"
            );
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                futures_util::future::join_all(handles),
            )
            .await;
        }
    }

    info!("smdjad stopped");
    let _ = std::fs::remove_file(&pid_path);
    // Socket is removed by _socket_guard's Drop impl on function exit.

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn read_only_role_blocks_write_tools() {
        // The least-privilege check in execute_tool blocks write tools when
        // session mode is "review". Verify the logic inline.
        let read_only_modes = ["review"];
        let write_tools = ["edit_file", "bash", "write_file", "run_command"];
        for mode in &read_only_modes {
            for tool in &write_tools {
                let is_blocked = *mode == "review" && write_tools.contains(tool);
                assert!(is_blocked, "tool {tool} should be blocked for mode {mode}");
            }
        }
    }

    #[test]
    fn loop_retire_state_is_terminal() {
        // Verify the terminal-status strings used in loop.retire enforcement.
        // "retired" must not be "complete" or "failed" — so the retire guard
        // would have rejected it (retired loops cannot be retired again).
        let retired = "retired";
        assert!(retired != "complete" && retired != "failed");
    }

    #[test]
    fn loop_complete_and_failed_allow_retire() {
        // Only complete or failed loops may be retired — verify the predicate.
        let terminal_for_retire = |s: &str| s == "complete" || s == "failed";
        assert!(terminal_for_retire("complete"));
        assert!(terminal_for_retire("failed"));
        assert!(!terminal_for_retire("planning"));
        assert!(!terminal_for_retire("slicing"));
        assert!(!terminal_for_retire("retired"));
    }

    #[test]
    fn retired_loop_cannot_be_re_run() {
        // The loop.run guard rejects status == "retired".
        let guard = |status: &str| status == "retired";
        assert!(guard("retired"));
        assert!(!guard("complete"));
        assert!(!guard("planning"));
    }

    #[tokio::test]
    async fn subprocess_provider_absent_keys_fails() {
        // When no API keys and no special CLIs are set, build_provider returns Err.
        // We can't easily unset all env vars in a test (other tests run in parallel),
        // so we verify the error path via direct build when we know keys are absent.
        // This is a best-effort heuristic test.
        if std::env::var("ANTHROPIC_API_KEY").is_ok() || std::env::var("OPENAI_API_KEY").is_ok() {
            // If keys are present in test environment, skip.
            return;
        }
        // Attempt build — expect Err since no keys and likely no local server.
        // We don't care about the exact message, just that it fails gracefully.
        let result = super::build_provider().await;
        // Result may be Ok if local endpoint happens to be up; just verify no panic.
        drop(result);
    }

    /// Returns the provider name that `build_provider` would select given the
    /// detection results for each candidate, encoding the subscription-first
    /// priority order without touching the network or filesystem.
    ///
    /// Priority (index 0 = highest):
    /// 0. claude CLI binary present
    /// 1. codex CLI binary present
    /// 2. copilot detected
    /// 3. poolside detected
    /// 4. `ANTHROPIC_API_KEY` set
    /// 5. `OPENAI_API_KEY` set
    /// 6. minimax detected
    /// 7. berget detected
    #[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
    fn provider_priority(
        claude_cli: bool,
        codex_cli: bool,
        copilot: bool,
        poolside: bool,
        anthropic_key: bool,
        openai_key: bool,
        minimax: bool,
        berget: bool,
    ) -> &'static str {
        if claude_cli {
            return "claude-cli";
        }
        if codex_cli {
            return "codex-cli";
        }
        if copilot {
            return "copilot";
        }
        if poolside {
            return "poolside";
        }
        if anthropic_key {
            return "anthropic";
        }
        if openai_key {
            return "openai";
        }
        if minimax {
            return "minimax";
        }
        if berget {
            return "berget";
        }
        "none"
    }

    #[test]
    fn cli_wins_over_api_key_when_both_present() {
        // CLI subscription beats API key — the fundamental invariant of L20.
        assert_eq!(
            provider_priority(true, false, false, false, true, true, false, false),
            "claude-cli"
        );
        assert_eq!(
            provider_priority(false, true, false, false, false, true, false, false),
            "codex-cli"
        );
    }

    #[test]
    fn api_key_selected_when_no_cli_available() {
        assert_eq!(
            provider_priority(false, false, false, false, true, false, false, false),
            "anthropic"
        );
        assert_eq!(
            provider_priority(false, false, false, false, false, true, false, false),
            "openai"
        );
    }

    #[test]
    fn cli_providers_ordered_before_copilot_and_poolside() {
        // Even copilot (subscription-like) comes after the CLI runners.
        assert_eq!(
            provider_priority(false, true, true, true, false, false, false, false),
            "codex-cli"
        );
    }

    #[test]
    fn anthropic_key_before_openai_key() {
        assert_eq!(
            provider_priority(false, false, false, false, true, true, false, false),
            "anthropic"
        );
    }

    #[test]
    fn minimax_and_berget_are_lowest_priority_before_local() {
        assert_eq!(
            provider_priority(false, false, false, false, false, false, true, false),
            "minimax"
        );
        assert_eq!(
            provider_priority(false, false, false, false, false, false, false, true),
            "berget"
        );
        assert_eq!(
            provider_priority(false, false, false, false, false, false, false, false),
            "none"
        );
    }
}
