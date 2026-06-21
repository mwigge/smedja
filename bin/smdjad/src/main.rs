use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
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

#[allow(clippy::too_many_lines)] // all six RPC methods live in one function per the spec
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
