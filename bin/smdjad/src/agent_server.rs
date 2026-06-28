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
use smedja_ingot::{IngotHandle, RollupTier};
use smedja_types::Timestamp;

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
pub async fn serve(listener: UnixListener, dispatcher: Arc<Dispatcher>, ingot: IngotHandle) {
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
            let Some(mut agent_event) = turn_event_to_agent_event(&event) else {
                continue;
            };
            // Enrich TurnEnd with the cumulative token-economy figure so the
            // st-statusbar EfficiencyModule can render it. Sourced from the
            // savings ledger; advisory — a query error leaves the fields None
            // and the segment simply does not render (no misleading zero).
            enrich_turn_end_savings(&mut agent_event, &ingot).await;
            let line = AgentEventEnvelope::new(agent_event).to_json_line();
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
fn turn_event_to_agent_event(event: &TurnEvent) -> Option<AgentEvent> {
    match event {
        TurnEvent::Started {
            session_id,
            turn_id,
            ..
        } => Some(AgentEvent::TurnStart {
            turn_id: Some(turn_id.clone()),
            session_id: Some(session_id.clone()),
        }),
        TurnEvent::ToolCalled {
            tool_name,
            input_summary,
            turn_id,
            ..
        } => Some(AgentEvent::ToolCall {
            turn_id: turn_id.clone(),
            tool: Some(tool_name.clone()),
            summary: Some(input_summary.clone()),
        }),
        TurnEvent::AssistantDelta {
            content, turn_id, ..
        }
        | TurnEvent::ThinkingDelta {
            content, turn_id, ..
        } => Some(AgentEvent::StreamDelta {
            turn_id: turn_id.clone(),
            content: Some(content.clone()),
        }),
        TurnEvent::Completed {
            session_id,
            turn_id,
            ..
        }
        | TurnEvent::Failed {
            session_id,
            turn_id,
            ..
        } => Some(AgentEvent::TurnEnd {
            turn_id: Some(turn_id.clone()),
            session_id: Some(session_id.clone()),
            // Filled in by enrich_turn_end_savings once the ledger is consulted.
            tokens_saved: None,
            efficiency_ratio: None,
        }),
        // Quality snapshots are internal signals — not surfaced to agent event consumers.
        TurnEvent::QualitySnapshot { .. } => None,
    }
}

/// Fills a [`AgentEvent::TurnEnd`]'s cumulative savings fields from the ledger.
///
/// Queries the session's all-source `tokens_saved` total and the daily-tier
/// efficiency ratio over the session's window. Advisory: a missing session id or
/// a ledger error leaves both fields `None`, so the status-bar segment renders
/// nothing rather than a misleading zero. A non-`TurnEnd` event is left
/// unchanged.
async fn enrich_turn_end_savings(event: &mut AgentEvent, ingot: &IngotHandle) {
    let AgentEvent::TurnEnd {
        session_id,
        tokens_saved,
        efficiency_ratio,
        ..
    } = event
    else {
        return;
    };
    let Some(sid) = session_id.as_deref() else {
        return;
    };
    if let Ok(total) = ingot.session_tokens_saved(sid).await {
        if total > 0 {
            *tokens_saved = u64::try_from(total).ok();
        }
    }
    // Efficiency ratio over the whole history (epoch → now), daily tier.
    let since = Timestamp::from_micros(0);
    let until = Timestamp::now();
    if let Ok(ratio) = ingot
        .efficiency_ratio(RollupTier::Daily, since, until)
        .await
    {
        if ratio > 0.0 {
            *efficiency_ratio = Some(ratio);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_bellows::event::CorrelationCtx;
    use std::path::Path;

    /// Maps a [`TurnEvent`] to an envelope wire line (pure mapper, no enrichment).
    /// Returns `None` for events that do not produce an agent event.
    fn turn_event_to_agent_event_line(event: &TurnEvent) -> Option<String> {
        turn_event_to_agent_event(event).map(|ev| AgentEventEnvelope::new(ev).to_json_line())
    }

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
        let line = turn_event_to_agent_event_line(&event).expect("must produce event");
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
            full_input: None,
            turn_id: Some("t1".into()),
            correlation: CorrelationCtx::default(),
            tool_call_id: None,
        };
        let env = AgentEventEnvelope::from_json_line(
            &turn_event_to_agent_event_line(&event).expect("must produce event"),
        )
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
        let env = AgentEventEnvelope::from_json_line(
            &turn_event_to_agent_event_line(&event).expect("must produce event"),
        )
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
        let env = AgentEventEnvelope::from_json_line(
            &turn_event_to_agent_event_line(&completed).expect("must produce event"),
        )
        .expect("must decode");
        assert_eq!(
            env.event,
            AgentEvent::TurnEnd {
                turn_id: Some("t1".into()),
                session_id: Some("s".into()),
                tokens_saved: None,
                efficiency_ratio: None,
            }
        );

        let failed = TurnEvent::Failed {
            session_id: "s".into(),
            turn_id: "t-fail".into(),
            reason: "timeout".into(),
            correlation: CorrelationCtx::default(),
        };
        let env = AgentEventEnvelope::from_json_line(
            &turn_event_to_agent_event_line(&failed).expect("must produce event"),
        )
        .expect("must decode");
        assert_eq!(
            env.event,
            AgentEvent::TurnEnd {
                turn_id: Some("t-fail".into()),
                session_id: Some("s".into()),
                tokens_saved: None,
                efficiency_ratio: None,
            }
        );
    }

    #[tokio::test]
    async fn enrich_turn_end_fills_cumulative_tokens_saved() {
        use smedja_ingot::{Ingot, TokensSavedEntry};
        use uuid::Uuid;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        ingot
            .insert_tokens_saved(TokensSavedEntry {
                id: Uuid::new_v4(),
                session_id: "sess-e".to_owned(),
                turn_n: 0,
                command: "cache_read".to_owned(),
                tokens_saved: 4242,
                source: "cache".to_owned(),
                created_at: Timestamp::now(),
            })
            .await
            .unwrap();

        let mut event = AgentEvent::TurnEnd {
            turn_id: Some("t1".to_owned()),
            session_id: Some("sess-e".to_owned()),
            tokens_saved: None,
            efficiency_ratio: None,
        };
        enrich_turn_end_savings(&mut event, &ingot).await;
        match event {
            AgentEvent::TurnEnd { tokens_saved, .. } => {
                assert_eq!(tokens_saved, Some(4242));
            }
            other => panic!("expected TurnEnd, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn enrich_leaves_fields_none_when_no_savings() {
        use smedja_ingot::Ingot;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let mut event = AgentEvent::TurnEnd {
            turn_id: Some("t1".to_owned()),
            session_id: Some("empty".to_owned()),
            tokens_saved: None,
            efficiency_ratio: None,
        };
        enrich_turn_end_savings(&mut event, &ingot).await;
        match event {
            AgentEvent::TurnEnd {
                tokens_saved,
                efficiency_ratio,
                ..
            } => {
                assert_eq!(tokens_saved, None, "no savings → no misleading zero");
                assert_eq!(efficiency_ratio, None);
            }
            other => panic!("expected TurnEnd, got {other:?}"),
        }
    }
}
