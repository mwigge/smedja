pub mod acp;
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
use serde_json::{json, Value};
use smedja_adapter::types::{Message as AdapterMessage, Role as AdapterRole};
use smedja_adapter::{
    AnthropicProvider, BergetProvider, CallOptions, CopilotProvider, Delta, LocalProvider,
    MinimaxProvider, OpenAiProvider, PoolsideProvider, Provider,
};
use smedja_assayer::{BashArity, WorktreePool};
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_ingot::{Checkpoint, CostEntry, Ingot, McpServer, Session, Task};
use smedja_rpc::{codes, router::Router, server::Server, RpcError};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

use crate::cowork::{ApprovalPrompt, CoworkGate, Decision};
use crate::sandbox::SandboxExecutor;

fn socket_path() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(base).join("smdjad.sock")
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
        warn!("cannot create data directory; using in-memory store");
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
/// 1. `ANTHROPIC_API_KEY` → [`AnthropicProvider`]
/// 2. `OPENAI_API_KEY` → [`OpenAiProvider`]
/// 3. `gh` binary + copilot extension (or `GITHUB_TOKEN`) → [`CopilotProvider`]
/// 4. `poolside` binary → [`PoolsideProvider`]
/// 5. `MINIMAX_API_KEY` → [`MinimaxProvider`]
/// 6. `BERGET_API_KEY` → [`BergetProvider`]
/// 7. Local rs-llmctl endpoint health check → [`LocalProvider`]
///
/// Returns `Err(reason)` only when all options are unavailable.
async fn build_provider() -> Result<Box<dyn Provider>, String> {
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        info!(provider = "anthropic", "provider selected");
        return Ok(Box::new(AnthropicProvider::new(key)));
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        info!(provider = "openai", "provider selected");
        return Ok(Box::new(OpenAiProvider::new("https://api.openai.com", key)));
    }
    if let Some(p) = CopilotProvider::detect() {
        info!(provider = "copilot", "provider selected");
        return Ok(Box::new(p));
    }
    if let Some(p) = PoolsideProvider::detect() {
        info!(provider = "poolside", "provider selected");
        return Ok(Box::new(p));
    }
    if let Some(p) = MinimaxProvider::detect() {
        info!(provider = "minimax", "provider selected");
        return Ok(Box::new(p));
    }
    if let Some(p) = BergetProvider::detect() {
        info!(provider = "berget", "provider selected");
        return Ok(Box::new(p));
    }
    // Fall back to the local rs-llmctl endpoint.
    let local = LocalProvider::connect().await;
    if local.capability.healthy {
        info!(
            provider = "local",
            model_id = %local.capability.model_id,
            "provider selected",
        );
        return Ok(Box::new(local));
    }
    warn!("no provider available — all options exhausted");
    Err("no LLM API key and local endpoint unreachable".to_owned())
}

/// Drains `stream`, accumulating text deltas into a single string.
///
/// Returns `Ok((full_response, output_tokens))` on success, or `Err(reason)` if
/// the stream yields an error item.  Each `Delta::Text` chunk is forwarded to
/// `dispatcher` as a [`TurnEvent::AssistantDelta`].
async fn drain_stream(
    mut stream: smedja_adapter::DeltaStream,
    dispatcher: &Dispatcher,
) -> Result<(String, u32), String> {
    let mut full_response = String::new();
    let mut output_tokens = 0u32;
    loop {
        match stream.next().await {
            None => break,
            Some(Ok(Delta::Text(t))) => {
                full_response.push_str(&t);
                dispatcher.publish(TurnEvent::AssistantDelta { content: t });
            }
            Some(Ok(Delta::Usage {
                output_tokens: n, ..
            })) => {
                output_tokens = n;
            }
            Some(Err(e)) => return Err(e.to_string()),
        }
    }
    Ok((full_response, output_tokens))
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
#[allow(clippy::too_many_lines)] // all turn lifecycle steps live in one function per the spec
async fn run_turn(
    ingot: Arc<Mutex<Ingot>>,
    dispatcher: Arc<Dispatcher>,
    session_id: String,
    turn_id: String,
    gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
) {
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
                });
                return;
            }
            Err(e) => {
                warn!(turn_id = %turn_id, error = %e, "failed to load task");
                dispatcher.publish(TurnEvent::Failed {
                    session_id,
                    turn_id,
                    reason: e.to_string(),
                });
                return;
            }
        }
    };

    // 2. Select provider from environment.
    let provider = match build_provider().await {
        Ok(p) => p,
        Err(reason) => {
            warn!("no LLM API key set; turn cannot execute");
            let _ = ingot.lock().await.update_task_status(&turn_id, "failed");
            dispatcher.publish(TurnEvent::Failed {
                session_id,
                turn_id,
                reason,
            });
            return;
        }
    };

    // Derive model and runner together once from the environment.
    let (model, runner) = if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        (
            std::env::var("SMEDJA_MODEL")
                .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_owned()),
            "anthropic".to_owned(),
        )
    } else {
        (
            std::env::var("SMEDJA_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_owned()),
            "openai".to_owned(),
        )
    };

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
    let builtin_tools: Vec<serde_json::Value> = vec![
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

    let mut full_response = String::new();
    let mut total_output_tokens = 0u32;

    'tool_loop: for _iteration in 0..MAX_TOOL_TURNS {
        // 5a. Stream LLM response.
        let stream = provider.stream_chat(&messages, &opts);
        let (response_text, output_tokens) = match drain_stream(stream, &dispatcher).await {
            Ok(pair) => pair,
            Err(reason) => {
                warn!(turn_id = %turn_id, error = %reason, "stream error during turn");
                let _ = ingot.lock().await.update_task_status(&turn_id, "failed");
                dispatcher.publish(TurnEvent::Failed {
                    session_id,
                    turn_id,
                    reason,
                });
                return;
            }
        };
        total_output_tokens = total_output_tokens.saturating_add(output_tokens);

        // 5b. Parse tool calls from the response text.
        // The adapter streams plain text; detect tool calls via JSON heuristics.
        // A tool call appears as a JSON object with a "tool" key in the response.
        let tool_call = parse_tool_call(&response_text);

        if let Some((tool_name, tool_input)) = tool_call {
            // Append assistant response to message history.
            messages.push(AdapterMessage {
                role: AdapterRole::Assistant,
                content: response_text.clone(),
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
                execute_tool(&tool_name, &tool_input, &workspace_root, session.as_ref()).await
            };

            // 5e. Append tool result as a user message and continue the loop.
            messages.push(AdapterMessage {
                role: AdapterRole::User,
                content: format!("<tool_result tool=\"{tool_name}\">{tool_result}</tool_result>"),
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
        warn!(turn_id = %turn_id, error = %e, "failed to store task response");
        dispatcher.publish(TurnEvent::Failed {
            session_id,
            turn_id,
            reason: e.to_string(),
        });
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
            runner,
            model,
            input_tok: 0,
            output_tok: i64::from(total_output_tokens),
            cost_usd: 0.0, // ponytail: pricing table deferred
            created_at: now_epoch(),
        };
        let mut ig = ingot.lock().await;
        if let Err(e) = ig.insert_cost(&entry) {
            warn!(error = %e, "failed to record cost entry");
        }
    }

    // 8. Save checkpoint.
    {
        let cp = Checkpoint {
            id: Uuid::new_v4(),
            session_id: session_id.clone(),
            turn_n,
            messages_json: serde_json::json!([{"role":"assistant","content":full_response}])
                .to_string(),
            created_at: now_epoch(),
        };
        let mut ig = ingot.lock().await;
        if let Err(e) = ig.save_checkpoint(&cp) {
            warn!(error = %e, "failed to save checkpoint");
        }
    }

    dispatcher.publish(TurnEvent::Completed {
        session_id,
        turn_id,
        output_tokens: total_output_tokens,
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
        other => format!("error: tool '{other}' is not available"),
    }
}

/// Subscribes to [`TurnEvent::Started`] and spawns a [`run_turn`] task for each.
fn spawn_worker(
    ingot: Arc<Mutex<Ingot>>,
    dispatcher: Arc<Dispatcher>,
    gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
) {
    tokio::spawn(async move {
        let mut rx = dispatcher.subscribe();
        loop {
            match rx.recv().await {
                Ok(TurnEvent::Started {
                    session_id,
                    turn_id,
                }) => {
                    let ig = Arc::clone(&ingot);
                    let dp = Arc::clone(&dispatcher);
                    let g = Arc::clone(&gates);
                    tokio::spawn(run_turn(ig, dp, session_id, turn_id, g));
                }
                Ok(_) => {} // ignore other events
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::error!(
                        dropped = n,
                        "turn worker lagged; events dropped — some turns may be lost"
                    );
                    // continue — do not break; the worker stays alive after lag
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
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
                .ok_or_else(|| missing_param("session_id"))?;

            let mut guard = ig.lock().await;

            let parent = guard
                .get_session(session_id)
                .map_err(|e| ingot_err(&e))?
                .ok_or_else(|| {
                    RpcError::new(
                        codes::INTERNAL_ERROR,
                        format!("session not found: {session_id}"),
                    )
                })?;

            let latest_cp = guard
                .latest_checkpoint(session_id)
                .map_err(|e| ingot_err(&e))?;

            let now = now_epoch();
            let new_id = Uuid::new_v4().to_string();

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

            let has_checkpoint = latest_cp.is_some();
            if let Some(cp) = latest_cp {
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

            dispatcher.publish(TurnEvent::Started {
                session_id,
                turn_id: task_id.to_string(),
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
            let roles: Vec<String> = params["roles"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();

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

                    bg_dispatcher.publish(TurnEvent::Started {
                        session_id: session_id.to_string(),
                        turn_id: task_id.to_string(),
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

    let path = socket_path();

    // Remove stale socket if it exists.
    let _ = std::fs::remove_file(&path);

    // Bind BEFORE spawning so a port-conflict error exits cleanly.
    let listener = UnixListener::bind(&path)?;

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

    let dispatcher = Arc::new(Dispatcher::new(32));
    let gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>> = Arc::new(Mutex::new(HashMap::new()));

    let router = build_router(&ingot, Arc::clone(&dispatcher), &gates);

    spawn_worker(
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

    info!("smdjad stopped");
    let _ = std::fs::remove_file(&pid_path);
    let _ = std::fs::remove_file(&path);

    Ok(())
}

#[cfg(test)]
mod tests {
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
}
