pub mod acp;
pub mod cowork;
pub mod mcp_http;
pub mod sandbox;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use serde_json::{json, Value};
use smedja_adapter::types::{Message as AdapterMessage, Role as AdapterRole};
use smedja_adapter::{
    AnthropicProvider, BergetProvider, CallOptions, CopilotProvider, Delta, LocalProvider,
    MinimaxProvider, OpenAiProvider, PoolsideProvider, Provider,
};
use smedja_assayer::WorktreePool;
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_ingot::{Checkpoint, CostEntry, Ingot, McpServer, Session, Task};
use smedja_rpc::{codes, router::Router, server::Server, RpcError};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

use crate::cowork::CoworkGate;

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
/// This guard is consulted by any bash tool handler before executing write-arity
/// commands (see `smedja_assayer::classify_bash` / `BashArity`).
#[allow(dead_code)] // ToolGate integration point: called by bash tool dispatch when implemented
fn role_allows_write_bash(session: &Session) -> bool {
    // ponytail: review role is read-only by default; all others are unrestricted
    session.mode.as_deref() != Some("review")
}

/// Executes a single turn: loads the task, calls the LLM, stores the response.
#[allow(clippy::too_many_lines)] // all turn lifecycle steps live in one function per the spec
async fn run_turn(
    ingot: Arc<Mutex<Ingot>>,
    dispatcher: Arc<Dispatcher>,
    session_id: String,
    turn_id: String,
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

    let model = if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        "claude-haiku-4-5-20251001"
    } else {
        "gpt-4o-mini"
    };

    // Inject active task context if the session has a task_id.
    let task_prefix = {
        let ig = ingot.lock().await;
        match ig.get_session(&session_id) {
            Ok(Some(session)) => {
                if let Some(ref task_id) = session.task_id {
                    match ig.get_task(task_id) {
                        Ok(Some(active_task)) => format!(
                            "\n\nActive task: {}\n{}",
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

    let opts = CallOptions {
        model: model.to_owned(),
        max_tokens: Some(2048),
        temperature: Some(0.7),
        system: Some(system_prompt),
    };
    let messages = vec![AdapterMessage {
        role: AdapterRole::User,
        content: task.title.clone(),
    }];

    // 3. Mark in_progress.
    {
        let mut ig = ingot.lock().await;
        if let Err(e) = ig.update_task_status(&turn_id, "in_progress") {
            warn!(turn_id = %turn_id, error = %e, "failed to mark task in_progress");
        }
    }

    // 4. Stream deltas.
    let stream = provider.stream_chat(&messages, &opts);
    let (full_response, output_tokens) = match drain_stream(stream, &dispatcher).await {
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

    // 5. Persist response and mark complete.
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

    // 6. Record cost entry.
    {
        let runner = if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            "anthropic"
        } else {
            "openai"
        };
        let entry = CostEntry {
            id: Uuid::new_v4(),
            session_id: session_id.clone(),
            turn_n: 0, // ponytail: sequential turn_n tracking not yet implemented
            runner: runner.to_owned(),
            model: model.to_owned(),
            input_tok: 0,
            output_tok: i64::from(output_tokens),
            cost_usd: 0.0, // ponytail: pricing table deferred
            created_at: now_epoch(),
        };
        let mut ig = ingot.lock().await;
        if let Err(e) = ig.insert_cost(&entry) {
            warn!(error = %e, "failed to record cost entry");
        }
    }

    // 7. Save checkpoint.
    {
        let cp = Checkpoint {
            id: Uuid::new_v4(),
            session_id: session_id.clone(),
            turn_n: 0, // ponytail: using 0 until turn_n tracking is added
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
        output_tokens,
    });
}

/// Subscribes to [`TurnEvent::Started`] and spawns a [`run_turn`] task for each.
fn spawn_worker(ingot: Arc<Mutex<Ingot>>, dispatcher: Arc<Dispatcher>) {
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
                    tokio::spawn(run_turn(ig, dp, session_id, turn_id));
                }
                Ok(_) => {}      // ignore other events
                Err(_) => break, // channel closed or lagged
            }
        }
    });
}

#[allow(clippy::too_many_lines)] // all RPC methods live in one function per the spec
fn build_router(ingot: &Arc<Mutex<Ingot>>, dispatcher: Arc<Dispatcher>) -> Router {
    let mut router = Router::new();

    let gate = Arc::new(CoworkGate::new());
    let pool = Arc::new(Mutex::new(WorktreePool::new()));

    // ── ping ────────────────────────────────────────────────────────────────
    router.register("ping", |_| async { Ok(json!("pong")) });

    // ── session.create ──────────────────────────────────────────────────────
    {
        let ig = Arc::clone(ingot);
        router.register("session.create", move |params: Value| {
            let ig = Arc::clone(&ig);
            async move {
                let title = params
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned);

                let now = now_epoch();
                let session = Session {
                    id: Uuid::new_v4(),
                    created_at: now,
                    updated_at: now,
                    status: "active".to_owned(),
                    task_id: None,
                    // Store the caller-supplied title in the `mode` field — Session
                    // has no dedicated title column; this is the nearest optional
                    // text field available on the existing schema.
                    mode: title.clone(),
                    cowork_mode: false,
                };

                ig.lock()
                    .await
                    .create_session(&session)
                    .map_err(|e| ingot_err(&e))?;

                Ok(json!({
                    "id": session.id,
                    "title": title,
                    "created_at": session.created_at,
                }))
            }
        });
    }

    // ── session.list ────────────────────────────────────────────────────────
    {
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
    }

    // ── session.get ─────────────────────────────────────────────────────────
    {
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
    }

    // ── session.delete ──────────────────────────────────────────────────────
    {
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
    }

    // ── task.get ────────────────────────────────────────────────────────────
    {
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
    }

    // ── task.list ───────────────────────────────────────────────────────────
    {
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
    }

    // ── task.create ─────────────────────────────────────────────────────────
    {
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
    }

    // ── task.close ──────────────────────────────────────────────────────────
    {
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
    }

    // ── turn.submit ─────────────────────────────────────────────────────────
    {
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
    }

    // ── session.checkpoint.list ─────────────────────────────────────────────
    {
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
    }

    // ── session.rollback ────────────────────────────────────────────────────
    {
        let ig = Arc::clone(ingot);
        router.register("session.rollback", move |params: Value| {
            let ig = Arc::clone(&ig);
            async move {
                let session_id = params
                    .get("session_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| missing_param("session_id"))?;
                let turn_n = params
                    .get("turn_n")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| missing_param("turn_n"))?;
                // turn_n is stored as i64 in the DB but load_checkpoint takes u32.
                let turn_u32 = u32::try_from(turn_n)
                    .map_err(|_| RpcError::new(codes::INVALID_PARAMS, "turn_n out of u32 range"))?;
                let cp = ig
                    .lock()
                    .await
                    .load_checkpoint(session_id, turn_u32)
                    .map_err(|e| ingot_err(&e))?
                    .ok_or_else(|| RpcError::new(codes::INTERNAL_ERROR, "checkpoint not found"))?;
                Ok(json!({ "turn_n": cp.turn_n, "messages_json": cp.messages_json }))
            }
        });
    }

    // ── session.cost ────────────────────────────────────────────────────────
    {
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
    }

    // ── cowork.set ──────────────────────────────────────────────────────────
    {
        let ig = Arc::clone(ingot);
        router.register("cowork.set", move |params: Value| {
            let ig = Arc::clone(&ig);
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
                Ok(json!({ "session_id": session_id, "cowork_mode": enabled }))
            }
        });
    }

    // ── mcp.register ────────────────────────────────────────────────────────
    {
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
    }

    // ── mcp.list ────────────────────────────────────────────────────────────
    {
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
    }

    // ── cowork.approve ───────────────────────────────────────────────────────
    {
        let gate = Arc::clone(&gate);
        router.register("cowork.approve", move |params: Value| {
            let gate = Arc::clone(&gate);
            async move {
                let id = params["id"]
                    .as_str()
                    .ok_or_else(|| missing_param("id"))?
                    .to_owned();
                let found = gate.approve(&id).await;
                Ok(json!({ "id": id, "resolved": found }))
            }
        });
    }

    // ── cowork.deny ──────────────────────────────────────────────────────────
    {
        let gate = Arc::clone(&gate);
        router.register("cowork.deny", move |params: Value| {
            let gate = Arc::clone(&gate);
            async move {
                let id = params["id"]
                    .as_str()
                    .ok_or_else(|| missing_param("id"))?
                    .to_owned();
                let reason = params["reason"].as_str().unwrap_or("denied").to_owned();
                let found = gate.deny(&id, reason).await;
                Ok(json!({ "id": id, "resolved": found }))
            }
        });
    }

    // ── cowork.modify ────────────────────────────────────────────────────────
    {
        let gate = Arc::clone(&gate);
        router.register("cowork.modify", move |params: Value| {
            let gate = Arc::clone(&gate);
            async move {
                let id = params["id"]
                    .as_str()
                    .ok_or_else(|| missing_param("id"))?
                    .to_owned();
                let instruction = params["instruction"].as_str().unwrap_or("").to_owned();
                let found = gate.modify(&id, instruction).await;
                Ok(json!({ "id": id, "resolved": found }))
            }
        });
    }

    // ── cowork.pending ───────────────────────────────────────────────────────
    {
        let gate = Arc::clone(&gate);
        router.register("cowork.pending", move |_: Value| {
            let gate = Arc::clone(&gate);
            async move {
                let pending = gate.list_pending().await;
                let out: Vec<Value> = pending
                    .into_iter()
                    .map(|(id, tool)| json!({ "id": id, "tool": tool }))
                    .collect();
                Ok(Value::Array(out))
            }
        });
    }

    // ── task.parallel ────────────────────────────────────────────────────────
    {
        let pool = Arc::clone(&pool);
        router.register("task.parallel", move |params: Value| {
            let pool = Arc::clone(&pool);
            async move {
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
                // ponytail: workspace root defaults to current dir; real path comes from session config later
                let workspace_root = std::path::PathBuf::from(".");
                let mut p = pool.lock().await;
                let ids: Vec<Value> = roles
                    .iter()
                    .map(|role| {
                        let id = p.register(role, &goal, &workspace_root);
                        json!({ "role": role, "task_id": id })
                    })
                    .collect();
                Ok(json!({ "goal": goal, "tasks": ids }))
            }
        });
    }

    // ── task.cancel ──────────────────────────────────────────────────────────
    {
        let pool = Arc::clone(&pool);
        router.register("task.cancel", move |params: Value| {
            let pool = Arc::clone(&pool);
            async move {
                let task_id = params["task_id"]
                    .as_str()
                    .ok_or_else(|| missing_param("task_id"))?
                    .to_owned();
                let found = pool.lock().await.cancel(&task_id);
                Ok(json!({ "task_id": task_id, "cancelled": found }))
            }
        });
    }

    // ── mcp.remove ───────────────────────────────────────────────────────────
    {
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
    }

    router
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let path = socket_path();

    // Remove stale socket if it exists.
    let _ = std::fs::remove_file(&path);

    let listener = UnixListener::bind(&path)?;
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
                    .iter()
                    .filter(|s| s.status == "in_flight")
                    .collect();
                if !orphaned.is_empty() {
                    tracing::warn!(
                        count = orphaned.len(),
                        "orphaned in_flight sessions detected at startup; marking as orphaned"
                    );
                    for sess in orphaned {
                        let _ = ig.update_session_status(&sess.id.to_string(), "orphaned");
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not list sessions at startup"),
        }
    }

    let dispatcher = Arc::new(Dispatcher::new(32));

    let router = build_router(&ingot, Arc::clone(&dispatcher));

    spawn_worker(Arc::clone(&ingot), Arc::clone(&dispatcher));

    // ACP HTTP server — activated by SMEDJA_ACP_PORT.
    if let Ok(port_str) = std::env::var("SMEDJA_ACP_PORT") {
        if let Ok(port) = port_str.parse::<u16>() {
            let acp_state = acp::AcpState {
                ingot: Arc::clone(&ingot),
            };
            let acp_router = acp::build_acp_router(acp_state);
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            tokio::spawn(async move {
                info!(%addr, "ACP HTTP server listening");
                if let Err(e) = axum::serve(
                    tokio::net::TcpListener::bind(addr)
                        .await
                        .expect("ACP bind failed"),
                    acp_router,
                )
                .await
                {
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
        _ = tokio::signal::ctrl_c() => {}
    }

    info!("smdjad stopped");
    let _ = std::fs::remove_file(&path);

    Ok(())
}
