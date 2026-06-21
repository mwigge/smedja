use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use serde_json::{json, Value};
use smedja_adapter::types::{Message as AdapterMessage, Role as AdapterRole};
use smedja_adapter::{AnthropicProvider, CallOptions, Delta, OpenAiProvider, Provider};
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_ingot::{Ingot, Session, Task};
use smedja_rpc::{codes, router::Router, server::Server, RpcError};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

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

/// Selects the LLM provider from environment variables.
///
/// Returns `Ok(provider)` when a key is present, or `Err(reason)` when neither
/// `ANTHROPIC_API_KEY` nor `OPENAI_API_KEY` is set.
fn build_provider() -> Result<Box<dyn Provider>, String> {
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        return Ok(Box::new(AnthropicProvider::new(key)));
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        return Ok(Box::new(OpenAiProvider::new("https://api.openai.com", key)));
    }
    Err("no LLM API key configured".to_owned())
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

/// Executes a single turn: loads the task, calls the LLM, stores the response.
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
    let provider = match build_provider() {
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
    let opts = CallOptions {
        model: model.to_owned(),
        max_tokens: Some(2048),
        temperature: Some(0.7),
        system: Some("You are smedja, an AI coding assistant.".to_owned()),
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
    let dispatcher = Arc::new(Dispatcher::new(32));

    let router = build_router(&ingot, Arc::clone(&dispatcher));

    spawn_worker(Arc::clone(&ingot), Arc::clone(&dispatcher));

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
