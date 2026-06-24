//! Agent-event push server.
//!
//! Binds `<rpc_sock>.agent` and streams [`AgentEventEnvelope`] JSON lines to
//! every connected terminal pane. The wire contract is owned by the
//! `smedja-agent-events` crate: each line is one versioned envelope wrapping a
//! tagged [`AgentEvent`].
//!
//! Protocol (one JSON object per line, server → client):
//!
//! ```text
//! {"schema_version":1,"type":"turn_start","turn_id":"…","session_id":"…"}
//! {"schema_version":1,"type":"tool_call","turn_id":"…","tool":"Bash","summary":"ls"}
//! {"schema_version":1,"type":"stream_delta","turn_id":"…","content":"hello"}
//! {"schema_version":1,"type":"turn_end","turn_id":"…","session_id":"…"}
//! ```
//!
//! Clients send a single subscribe line on connect, then receive events:
//!
//! ```text
//! {"method":"subscribe_pane","params":{"pane_id":"<uuid>"}}\n
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, Mutex};

use smedja_agent_events::{AgentEvent, AgentEventEnvelope};
use smedja_bellows::{Dispatcher, TurnEvent};

type SubList = Arc<Mutex<Vec<mpsc::Sender<String>>>>;

/// Returns the agent socket path: `<rpc_path>.agent`.
#[must_use]
pub fn agent_socket_path(rpc_path: &Path) -> PathBuf {
    let name = rpc_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let mut p = rpc_path.to_path_buf();
    p.set_file_name(format!("{name}.agent"));
    p
}

/// Accept connections on `listener` and fan out [`TurnEvent`]s to all panes.
///
/// Each connected pane receives every event; panes that disconnect are silently
/// removed from the subscriber list.
pub async fn serve(listener: UnixListener, dispatcher: Arc<Dispatcher>) {
    let subs: SubList = Arc::new(Mutex::new(Vec::new()));

    // Subscribe before spawning to avoid losing events published between
    // spawn() and the task's first await point.
    let mut rx = dispatcher.subscribe();
    let subs_bg = Arc::clone(&subs);
    tokio::spawn(async move {
        loop {
            let event = match rx.recv().await {
                Ok(ev) => ev,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "agent fanout lagged; events dropped");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let line = turn_event_to_agent_event_line(&event);
            let mut locked = subs_bg.lock().await;
            locked.retain(|tx| tx.try_send(line.clone()).is_ok());
        }
    });

    let semaphore = Arc::new(tokio::sync::Semaphore::new(32));
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "agent socket accept error");
                continue;
            }
        };
        let subs = Arc::clone(&subs);
        let sem = Arc::clone(&semaphore);
        tokio::spawn(async move {
            let Ok(permit) = sem.acquire_owned().await else {
                return;
            };
            handle_connection(stream, subs).await;
            drop(permit);
        });
    }
}

async fn handle_connection(stream: UnixStream, subs: SubList) {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    // Read the subscribe_pane request (required but not validated further).
    let mut line = String::new();
    if reader.read_line(&mut line).await.is_err() || line.is_empty() {
        return;
    }

    let (tx, mut rx) = mpsc::channel::<String>(64);
    subs.lock().await.push(tx);

    while let Some(event_line) = rx.recv().await {
        let mut buf = event_line;
        buf.push('\n');
        if writer.write_all(buf.as_bytes()).await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
            break;
        }
    }
}

/// Maps a Bellows [`TurnEvent`] onto the versioned agent-event wire contract.
///
/// The mapping is:
/// - [`TurnEvent::Started`] → [`AgentEvent::TurnStart`]
/// - [`TurnEvent::ToolCalled`] → [`AgentEvent::ToolCall`]
/// - [`TurnEvent::AssistantDelta`] → [`AgentEvent::StreamDelta`]
/// - [`TurnEvent::Completed`] / [`TurnEvent::Failed`] → [`AgentEvent::TurnEnd`]
fn turn_event_to_agent_event(event: &TurnEvent) -> AgentEvent {
    match event {
        TurnEvent::Started {
            session_id,
            turn_id,
            ..
        } => AgentEvent::TurnStart {
            turn_id: Some(turn_id.clone()),
            session_id: Some(session_id.clone()),
        },
        TurnEvent::ToolCalled {
            tool_name,
            input_summary,
            turn_id,
            ..
        } => AgentEvent::ToolCall {
            turn_id: turn_id.clone(),
            tool: Some(tool_name.clone()),
            summary: Some(input_summary.clone()),
        },
        TurnEvent::AssistantDelta {
            content, turn_id, ..
        } => AgentEvent::StreamDelta {
            turn_id: turn_id.clone(),
            content: Some(content.clone()),
        },
        TurnEvent::Completed {
            session_id,
            turn_id,
            ..
        }
        | TurnEvent::Failed {
            session_id,
            turn_id,
            ..
        } => AgentEvent::TurnEnd {
            turn_id: Some(turn_id.clone()),
            session_id: Some(session_id.clone()),
        },
    }
}

/// Converts a [`TurnEvent`] into a single newline-free JSON line wrapped in an
/// [`AgentEventEnvelope`] carrying the current schema version.
fn turn_event_to_agent_event_line(event: &TurnEvent) -> String {
    AgentEventEnvelope::new(turn_event_to_agent_event(event)).to_json_line()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_bellows::event::CorrelationCtx;
    use std::path::Path;

    #[test]
    fn agent_socket_path_appends_dot_agent() {
        let p = agent_socket_path(Path::new("/run/smdjad.sock"));
        assert_eq!(p, std::path::PathBuf::from("/run/smdjad.sock.agent"));
    }

    #[test]
    fn started_maps_to_turn_start_envelope() {
        let event = TurnEvent::Started {
            session_id: "sess-1".into(),
            turn_id: "turn-1".into(),
            correlation: CorrelationCtx {
                trace_id: Some("abc".into()),
                ..CorrelationCtx::default()
            },
        };
        let line = turn_event_to_agent_event_line(&event);
        assert!(!line.contains('\n'), "wire line must be newline-free");
        let env = AgentEventEnvelope::from_json_line(&line).expect("must decode");
        assert_eq!(
            env.schema_version,
            smedja_agent_events::CURRENT_SCHEMA_VERSION
        );
        assert_eq!(
            env.event,
            AgentEvent::TurnStart {
                turn_id: Some("turn-1".into()),
                session_id: Some("sess-1".into()),
            }
        );
    }

    #[test]
    fn tool_called_maps_to_tool_call() {
        let event = TurnEvent::ToolCalled {
            tool_name: "Bash".into(),
            input_summary: "ls -la".into(),
            turn_id: Some("t1".into()),
            correlation: CorrelationCtx::default(),
            tool_call_id: None,
        };
        let env = AgentEventEnvelope::from_json_line(&turn_event_to_agent_event_line(&event))
            .expect("must decode");
        assert_eq!(
            env.event,
            AgentEvent::ToolCall {
                turn_id: Some("t1".into()),
                tool: Some("Bash".into()),
                summary: Some("ls -la".into()),
            }
        );
    }

    #[test]
    fn assistant_delta_maps_to_stream_delta() {
        let event = TurnEvent::AssistantDelta {
            content: "hello".into(),
            turn_id: Some("t1".into()),
            correlation: CorrelationCtx::default(),
        };
        let env = AgentEventEnvelope::from_json_line(&turn_event_to_agent_event_line(&event))
            .expect("must decode");
        assert_eq!(
            env.event,
            AgentEvent::StreamDelta {
                turn_id: Some("t1".into()),
                content: Some("hello".into()),
            }
        );
    }

    #[test]
    fn completed_and_failed_map_to_turn_end() {
        let completed = TurnEvent::Completed {
            session_id: "s".into(),
            turn_id: "t1".into(),
            output_tokens: 88,
            input_tokens: Some(412),
            traceparent: Some("00-abc123def456-0102030405060708-01".into()),
            correlation: CorrelationCtx::default(),
        };
        let env = AgentEventEnvelope::from_json_line(&turn_event_to_agent_event_line(&completed))
            .expect("must decode");
        assert_eq!(
            env.event,
            AgentEvent::TurnEnd {
                turn_id: Some("t1".into()),
                session_id: Some("s".into()),
            }
        );

        let failed = TurnEvent::Failed {
            session_id: "s".into(),
            turn_id: "t-fail".into(),
            reason: "timeout".into(),
            correlation: CorrelationCtx::default(),
        };
        let env = AgentEventEnvelope::from_json_line(&turn_event_to_agent_event_line(&failed))
            .expect("must decode");
        assert_eq!(
            env.event,
            AgentEvent::TurnEnd {
                turn_id: Some("t-fail".into()),
                session_id: Some("s".into()),
            }
        );
    }
}
