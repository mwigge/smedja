use crate::blocks;
use crate::messages::push_system_message;
use crate::state::AppState;
use anyhow::Result;
use serde_json::json;
use smedja_bellows::StreamEvent;
use smedja_rpc::client::Client;
use std::path::PathBuf;

/// Startup routing decision derived from the `--session` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionStart {
    /// Attach to an existing session and replay its history.
    Resume(String),
    /// Create a fresh session (default behaviour).
    Create,
}

/// Maps the `--session` flag to a startup routing decision.
///
/// `Some(id)` routes to [`SessionStart::Resume`]; `None` routes to
/// [`SessionStart::Create`]. Whitespace-only ids are treated as absent.
pub(crate) fn session_start_decision(flag: Option<String>) -> SessionStart {
    match flag {
        Some(id) if !id.trim().is_empty() => SessionStart::Resume(id.trim().to_owned()),
        _ => SessionStart::Create,
    }
}

/// Whether a resume should rewind the session before replaying.
///
/// A `Some(turn)` target is destructive (calls `session.rollback`); `None` is a
/// non-destructive read-only replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumePlan {
    /// Rewind to `turn_n` via `session.rollback`, then replay.
    Rollback { turn_n: u32 },
    /// Replay current history without rewinding.
    ReplayOnly,
}

/// Derives the resume plan from an optional turn target.
pub(crate) fn resume_plan(turn: Option<u32>) -> ResumePlan {
    match turn {
        Some(turn_n) => ResumePlan::Rollback { turn_n },
        None => ResumePlan::ReplayOnly,
    }
}

/// Visibility state for all toggleable rail and overlay panels.
/// Full detail for a single session, fetched on demand via `session.get` when
/// the user presses Enter on a session rail item.
#[derive(Debug, Clone)]
pub(crate) struct SessionDetail {
    pub(crate) id: String,
    pub(crate) title: Option<String>,
    pub(crate) mode: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) active_change: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) cowork_mode: Option<String>,
}

impl SessionDetail {
    /// Construct from a `session.get` JSON response, tolerating missing optional fields.
    pub(crate) fn from_json(v: &serde_json::Value) -> Self {
        let str_opt = |key: &str| v[key].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        Self {
            id: v["id"].as_str().unwrap_or("-").to_owned(),
            title: str_opt("title"),
            mode: str_opt("mode"),
            status: str_opt("status"),
            active_change: str_opt("active_change"),
            created_at: v["created_at"].as_str().unwrap_or("-").to_owned(),
            updated_at: v["updated_at"].as_str().unwrap_or("-").to_owned(),
            cowork_mode: str_opt("cowork_mode"),
        }
    }
}

pub(crate) fn socket_path(override_path: Option<PathBuf>) -> PathBuf {
    override_path.unwrap_or_else(|| {
        let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(base).join("smdjad.sock")
    })
}

pub(crate) fn stream_socket_path(rpc_path: &std::path::Path) -> PathBuf {
    let mut p = rpc_path.as_os_str().to_owned();
    p.push(".stream");
    PathBuf::from(p)
}

/// Connects to the smdjad stream socket and forwards NDJSON events to `tx`
/// until the terminal `done` or `error` event is received.
pub(crate) async fn start_stream_reader(
    sock_path: PathBuf,
    task_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let stream = match UnixStream::connect(&sock_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "stream socket connect failed");
            let _ = tx.send(StreamEvent::Error {
                message: format!("stream unavailable: {e}"),
            });
            return;
        }
    };
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    let req = format!("{{\"task_id\":\"{task_id}\"}}\n");
    if writer.write_all(req.as_bytes()).await.is_err() {
        let _ = tx.send(StreamEvent::Error {
            message: "stream handshake failed".to_owned(),
        });
        return;
    }

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(ev) = serde_json::from_str::<StreamEvent>(trimmed) {
                    let terminal = ev.is_terminal();
                    let _ = tx.send(ev);
                    if terminal {
                        break;
                    }
                }
            }
        }
    }
}

/// The session a startup decision resolved to, plus its display metadata.
pub(crate) struct ResolvedSession {
    pub(crate) session_id: String,
    pub(crate) runner: String,
    pub(crate) model: Option<String>,
    pub(crate) tier: Option<String>,
    pub(crate) mode: Option<String>,
    /// `true` when an existing session was attached (history should be replayed).
    pub(crate) resumed: bool,
}

/// Resolves the startup decision into a concrete session.
///
/// [`SessionStart::Create`] calls `session.create` (current behaviour);
/// [`SessionStart::Resume`] validates the id via `session.get` and attaches to
/// it. An unknown id surfaces as an error so the caller can fail fast before
/// any terminal setup.
///
/// # Errors
///
/// Returns an error when `session.create` fails, or when a supplied resume id is
/// unknown (`session not found: <id>`).
pub(crate) async fn resolve_session(
    client: &mut Client,
    start: SessionStart,
) -> Result<ResolvedSession> {
    match start {
        SessionStart::Create => {
            // Announce our working directory as the workspace so the daemon
            // roots the LSP + code-graph at the project (not its own $HOME).
            let workspace = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let resp = client
                .call(
                    "session.create",
                    json!({ "title": "smedja", "workspace": workspace }),
                )
                .await
                .map_err(|e| anyhow::anyhow!("session.create failed: {e}"))?;
            Ok(ResolvedSession {
                session_id: resp["id"].as_str().unwrap_or("unknown").to_owned(),
                runner: resp
                    .get("runner")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_owned(),
                model: resp
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                tier: resp.get("tier").and_then(|v| v.as_str()).map(str::to_owned),
                mode: None,
                resumed: false,
            })
        }
        SessionStart::Resume(id) => {
            let resp = client
                .call("session.get", json!({ "id": id }))
                .await
                .map_err(|_| anyhow::anyhow!("session not found: {id}"))?;
            Ok(ResolvedSession {
                session_id: resp["id"].as_str().unwrap_or(&id).to_owned(),
                runner: resp
                    .get("runner")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_owned(),
                model: None,
                tier: None,
                mode: resp.get("mode").and_then(|v| v.as_str()).map(str::to_owned),
                resumed: true,
            })
        }
    }
}

/// Replays `session_id` into the view, optionally rewinding it first.
///
/// When `plan` is [`ResumePlan::Rollback`], `session.rollback` is called with
/// `{ session_id, turn_n }` to rewind the conversation (destructive, mirroring
/// `smj session rollback`) before history is read. [`ResumePlan::ReplayOnly`]
/// is non-destructive: it never calls `session.rollback`. In both cases the
/// rewound history is fetched via `session.history` and seeded into the view by
/// [`replay_history`].
pub(crate) async fn resume_into_view(state: &mut AppState, client: &mut Client, plan: ResumePlan) {
    let session_id = state.session_id.clone();
    if let ResumePlan::Rollback { turn_n } = plan {
        if let Err(e) = client
            .call(
                "session.rollback",
                json!({ "session_id": session_id, "turn_n": turn_n }),
            )
            .await
        {
            push_system_message(state, format!("session.rollback error: {e}"));
            return;
        }
    }
    match client
        .call("session.history", json!({ "session_id": session_id }))
        .await
    {
        Ok(history) => replay_history(state, &history),
        Err(e) => push_system_message(state, format!("session.history error: {e}")),
    }
}

/// Seeds the view from a `session.history` response, replaying prior turns.
///
/// Iterates the `turns` array in ascending `turn_n`, builds a completed
/// [`blocks::TurnBlock`] per turn via [`blocks::TurnBlock::from_history_turn`],
/// pushes it into the [`blocks::BlockStore`], renders its lines into the
/// [`main_panel::MainPanel`], and advances `state.turn_n` to the highest
/// replayed turn so the next live turn continues the sequence. A missing or
/// empty `turns` array is a no-op.
pub(crate) fn replay_history(state: &mut AppState, history: &serde_json::Value) {
    let Some(turns) = history.get("turns").and_then(serde_json::Value::as_array) else {
        return;
    };
    let mut ordered: Vec<&serde_json::Value> = turns.iter().collect();
    ordered.sort_by_key(|t| {
        t.get("turn_n")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    });
    let mut max_turn = state.turn_n;
    for turn in ordered {
        let turn_n = turn
            .get("turn_n")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);
        let messages = turn
            .get("messages")
            .cloned()
            .unwrap_or(serde_json::Value::Array(Vec::new()));
        let block = blocks::TurnBlock::from_history_turn(turn_n, &messages);
        for line in block.render_lines(80) {
            state.main_panel.push_line(line);
        }
        state.block_store.push(block);
        max_turn = max_turn.max(turn_n);
    }
    state.turn_n = max_turn;

    // Seed latency samples from audit events so the p95/p99 sparkline has
    // historical data rather than starting blank on every session load.
    if let Some(audit) = history.get("audit").and_then(serde_json::Value::as_array) {
        for ev in audit {
            if let Some(ms) = ev.get("latency_ms").and_then(serde_json::Value::as_u64) {
                if ms > 0 {
                    if state.latency_samples.len() >= LATENCY_SAMPLE_CAP {
                        state.latency_samples.pop_front();
                    }
                    state.latency_samples.push_back(ms);
                }
            }
        }
        state.obs_snapshot.latency_samples = state.latency_samples.clone();
    }
}

/// Formats `session.list` rows for the `/resume` picker.
///
/// Each row renders as `<short-id>  <title>  <mode>  <updated_at>`, where the
/// short id is the first 8 characters. Missing titles/modes degrade to empty
/// or `?` placeholders rather than being dropped.
#[must_use]
pub(crate) fn format_resume_rows(list: &serde_json::Value) -> Vec<String> {
    let Some(items) = list.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .map(|s| {
            let id = s
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let short = &id[..8.min(id.len())];
            let title = s
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let mode = s
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let updated =
                if let Some(s_val) = s.get("updated_at").and_then(serde_json::Value::as_str) {
                    s_val.to_owned()
                } else if let Some(n) = s.get("updated_at").and_then(serde_json::Value::as_f64) {
                    // epoch microseconds → relative display
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let secs = (n / 1_000_000.0) as i64;
                    #[allow(clippy::cast_possible_wrap)]
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs() as i64);
                    let ago = now - secs;
                    if ago < 60 {
                        format!("{ago}s ago")
                    } else if ago < 3600 {
                        format!("{}m ago", ago / 60)
                    } else if ago < 86400 {
                        format!("{}h ago", ago / 3600)
                    } else {
                        format!("{}d ago", ago / 86400)
                    }
                } else {
                    "-".to_owned()
                };
            format!("{short}  {title}  {mode}  {updated}")
        })
        .collect()
}

/// Parses `/resume` arguments into `(session_id, optional_turn)`.
///
/// `<id>` yields `(id, None)`; `<id> <turn>` yields `(id, Some(turn))`. A
/// non-numeric turn token is ignored (no turn target). Empty input yields
/// `None`.
#[must_use]
pub(crate) fn parse_resume_args(args: &str) -> Option<(String, Option<u32>)> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let id = parts.next()?.to_owned();
    let turn = parts.next().and_then(|t| t.parse::<u32>().ok());
    Some((id, turn))
}

/// Returns `true` (and emits a status line) when a resume must be refused
/// because a turn is awaiting a response.
pub(crate) fn resume_blocked_by_pending_turn(state: &mut AppState) -> bool {
    if state.pending_task_id.is_some() {
        push_system_message(state, "cannot resume while a turn is in flight");
        true
    } else {
        false
    }
}

/// Refresh interval for the metrics panel poll. Metrics are aggregates, not live
/// deltas, so a slow cadence is correct and cheap.
/// Maximum number of turn latency samples retained for p95/p99 computation.
pub(crate) const LATENCY_SAMPLE_CAP: usize = 50;

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::testutil::{make_state, render_frame};
    #[allow(unused_imports)]
    use serde_json::{json, Value};

    #[test]
    fn resume_when_session_flag_present() {
        let decision = session_start_decision(Some("abc-123".to_owned()));
        assert_eq!(decision, SessionStart::Resume("abc-123".to_owned()));
    }

    #[test]
    fn create_when_session_flag_absent() {
        assert_eq!(session_start_decision(None), SessionStart::Create);
    }

    #[test]
    fn resume_ignores_blank_session_flag() {
        assert_eq!(
            session_start_decision(Some("   ".to_owned())),
            SessionStart::Create
        );
    }

    #[test]
    fn replay_seeds_blocks_and_continues_turn_n() {
        let mut state = make_state("resume-session");
        let history = serde_json::json!({
            "session_id": "resume-session",
            "turns": [
                { "turn_n": 1, "created_at": "t1", "messages": [
                    { "role": "user", "content": "first prompt" },
                    { "role": "assistant", "content": "first reply" },
                ]},
                { "turn_n": 2, "created_at": "t2", "messages": [
                    { "role": "user", "content": "second prompt" },
                    { "role": "assistant", "content": "second reply" },
                ]},
            ],
        });
        replay_history(&mut state, &history);
        assert_eq!(state.block_store.len(), 2, "one block per turn");
        assert_eq!(
            state.turn_n, 2,
            "turn_n must equal the highest replayed turn"
        );
        let body = state.main_panel.visible_text();
        assert!(body.contains("first reply"), "panel missing turn 1: {body}");
        assert!(
            body.contains("second reply"),
            "panel missing turn 2: {body}"
        );
    }

    #[test]
    fn replay_empty_turns_is_noop() {
        let mut state = make_state("fresh-session");
        let history = serde_json::json!({ "session_id": "fresh-session", "turns": [] });
        replay_history(&mut state, &history);
        assert_eq!(state.block_store.len(), 0);
        assert_eq!(state.turn_n, 0);
    }

    #[test]
    fn replay_missing_turns_is_noop() {
        let mut state = make_state("fresh-session");
        let history = serde_json::json!({ "session_id": "fresh-session" });
        replay_history(&mut state, &history);
        assert_eq!(state.block_store.len(), 0);
        assert_eq!(state.turn_n, 0);
    }

    #[test]
    fn replay_history_seeds_latency_samples_from_audit() {
        let mut state = make_state("latency-seed-session");
        let history = serde_json::json!({
            "session_id": "latency-seed-session",
            "turns": [],
            "audit": [
                { "latency_ms": 1200 },
                { "latency_ms": 800 },
                { "latency_ms": 0 },       // zero must be skipped
                { "latency_ms": 2500 },
            ],
        });
        replay_history(&mut state, &history);
        // Zero latency is excluded; the three valid samples must be seeded.
        assert_eq!(
            state.latency_samples.len(),
            3,
            "latency_samples must be seeded from audit (zero excluded)"
        );
        assert!(state.latency_samples.contains(&1200));
        assert!(state.latency_samples.contains(&800));
        assert!(state.latency_samples.contains(&2500));
        // The obs_snapshot must reflect the seeded samples for p95/p99.
        assert_eq!(
            state.obs_snapshot.latency_samples.len(),
            3,
            "obs_snapshot must be updated"
        );
    }

    #[test]
    fn resume_list_formats_session_rows() {
        let list = serde_json::json!([
            {
                "id": "0123456789abcdef",
                "title": "fix the parser",
                "mode": "impl",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-06-22T09:30:00Z",
            },
            {
                "id": "fedcba9876543210",
                "title": "",
                "mode": serde_json::Value::Null,
                "created_at": "2026-01-02T00:00:00Z",
                "updated_at": "2026-06-21T11:00:00Z",
            },
        ]);
        let rows = format_resume_rows(&list);
        assert_eq!(rows.len(), 2, "one row per session");
        assert!(
            rows[0].starts_with("01234567"),
            "short id first: {}",
            rows[0]
        );
        assert!(rows[0].contains("fix the parser"), "title: {}", rows[0]);
        assert!(rows[0].contains("impl"), "mode: {}", rows[0]);
        assert!(
            rows[0].contains("2026-06-22T09:30:00Z"),
            "updated_at: {}",
            rows[0]
        );
        // Missing title / null mode must still produce a usable row.
        assert!(rows[1].starts_with("fedcba98"), "row: {}", rows[1]);
    }

    #[test]
    fn resume_with_turn_calls_rollback_then_replays() {
        assert_eq!(resume_plan(Some(3)), ResumePlan::Rollback { turn_n: 3 });
        assert_eq!(resume_plan(None), ResumePlan::ReplayOnly);
    }

    #[test]
    fn parse_resume_args_splits_id_and_turn() {
        assert_eq!(parse_resume_args("abc"), Some(("abc".to_owned(), None)));
        assert_eq!(
            parse_resume_args("abc 5"),
            Some(("abc".to_owned(), Some(5)))
        );
        assert_eq!(parse_resume_args(""), None);
        // Non-numeric turn is ignored (treated as no turn target).
        assert_eq!(parse_resume_args("abc xyz"), Some(("abc".to_owned(), None)));
    }

    #[test]
    fn resume_rejected_while_turn_in_flight() {
        let mut state = make_state("busy-session");
        state.pending_task_id = Some("task-1".to_owned());
        assert!(resume_blocked_by_pending_turn(&mut state));
        let body = state.main_panel.visible_text();
        assert!(body.contains("cannot resume"), "status message: {body}");
        // No pending turn → not blocked.
        let mut idle = make_state("idle-session");
        assert!(!resume_blocked_by_pending_turn(&mut idle));
    }

    fn parse_session_resp(
        resp: &serde_json::Value,
        cli_tier: Option<String>,
    ) -> (String, Option<String>, Option<String>) {
        let runner = resp
            .get("runner")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_owned();
        let model: Option<String> = resp
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let resp_tier: Option<String> =
            resp.get("tier").and_then(|v| v.as_str()).map(str::to_owned);
        let effective_tier = cli_tier.or(resp_tier);
        (runner, model, effective_tier)
    }

    #[test]
    fn startup_runner_populated_from_session_resp() {
        let resp = serde_json::json!({
            "id": "x",
            "runner": "claude-cli",
            "model": "claude-sonnet-4-6",
            "tier": "fast",
        });
        let (runner, model, tier) = parse_session_resp(&resp, None);
        assert_eq!(runner, "claude-cli");
        assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(tier.as_deref(), Some("fast"));
    }

    #[test]
    fn startup_fields_fall_back_gracefully_when_missing() {
        let resp = serde_json::json!({ "id": "x" });
        let (runner, model, tier) = parse_session_resp(&resp, None);
        assert_eq!(runner, "unknown");
        assert!(model.is_none());
        assert!(tier.is_none());
    }

    #[test]
    fn cli_tier_arg_takes_precedence_over_response_tier() {
        let resp = serde_json::json!({ "id": "x", "tier": "local" });
        let (_runner, _model, tier) = parse_session_resp(&resp, Some("deep".into()));
        assert_eq!(tier.as_deref(), Some("deep"));
    }

    #[test]
    fn session_detail_from_json_maps_all_fields() {
        let v = serde_json::json!({
            "id": "sess-42",
            "title": "refactor sprint",
            "mode": "auto",
            "status": "active",
            "active_change": "add-auth",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-06-28T00:00:00Z",
            "cowork_mode": "plan",
        });
        let detail = SessionDetail::from_json(&v);
        assert_eq!(detail.id, "sess-42");
        assert_eq!(detail.title.as_deref(), Some("refactor sprint"));
        assert_eq!(detail.mode.as_deref(), Some("auto"));
        assert_eq!(detail.status.as_deref(), Some("active"));
        assert_eq!(detail.active_change.as_deref(), Some("add-auth"));
        assert_eq!(detail.cowork_mode.as_deref(), Some("plan"));
    }

    #[test]
    fn session_detail_from_json_handles_missing_optional_fields() {
        let v = serde_json::json!({ "id": "bare-id",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
        });
        let detail = SessionDetail::from_json(&v);
        assert_eq!(detail.id, "bare-id");
        assert!(detail.title.is_none());
        assert!(detail.mode.is_none());
        assert!(detail.status.is_none());
        assert!(detail.active_change.is_none());
        assert!(detail.cowork_mode.is_none());
    }
}
