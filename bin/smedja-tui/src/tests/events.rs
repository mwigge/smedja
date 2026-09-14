//! `events`-area unit tests (moved verbatim from the former `tests.rs`).

use smedja_bellows::StreamEvent;

use crate::events::apply_stream_event;
use crate::state::{Message, Role};
use crate::test_support::{make_state, render_frame};
use crate::{
    push_system_message, replay_history, session_start_decision, SessionStart, MESSAGE_HISTORY_CAP,
};

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

// Bug regression: mid-stream `Usage` events must update the obs panel's
// throughput bar live, before the turn's `Done` commits the totals. Providers
// split usage across events (input on message_start, output on message_delta),
// so a per-field high-water mark is added on top of the committed session totals.
#[test]
fn usage_event_feeds_obs_throughput_live() {
    let mut state = make_state("usage-obs");
    // Two prior turns already committed into the session counters.
    state.session_tokens_in = 100;
    state.session_tokens_out = 200;
    let mut save = None;

    // message_start-style event: input known, output still zero.
    apply_stream_event(
        &mut state,
        StreamEvent::Usage {
            input_tok: 40,
            output_tok: 0,
        },
        &mut save,
    );
    // message_delta-style event: output known, input reported zero. The zero
    // must not clobber the earlier non-zero input.
    apply_stream_event(
        &mut state,
        StreamEvent::Usage {
            input_tok: 0,
            output_tok: 55,
        },
        &mut save,
    );

    assert_eq!(
        state.obs_snapshot.tokens_input, 140,
        "obs input = committed 100 + live 40"
    );
    assert_eq!(
        state.obs_snapshot.tokens_output, 255,
        "obs output = committed 200 + live 55 (zero input event must not reset)"
    );
}

// Bug 2 regression: an external CLI runner (codex/claude) reports each shell
// tool call as a structured `ToolCall` (which becomes the collapsed card) plus a
// `↳ ok · [<cmd>]` result delta that merely echoes the command. Rendering both
// doubled every tool call. The echo must be dropped so exactly one dim card line
// survives per call.
#[test]
fn external_tool_call_renders_single_collapsed_line_not_two() {
    let mut state = make_state("ext-tool");
    // Prior assistant text so the tool result doesn't open a fresh author chip.
    state.assistant_open = true;
    let mut save = None;

    apply_stream_event(
        &mut state,
        StreamEvent::ToolCall {
            name: "shell".into(),
            input: "git status".into(),
            full: Some("git status".into()),
        },
        &mut save,
    );
    apply_stream_event(
        &mut state,
        StreamEvent::Delta {
            text: "\n\u{21b3} ok \u{00b7} [git status]\n".into(),
        },
        &mut save,
    );

    let texts = state
        .main_panel
        .lines_text(0, state.main_panel.len().saturating_sub(1));
    let echo_lines = texts.iter().filter(|t| t.contains("\u{21b3} ok")).count();
    assert_eq!(
        echo_lines, 0,
        "redundant ok echo must be dropped; lines: {texts:?}"
    );
    let card_lines = texts.iter().filter(|t| t.contains("git status")).count();
    assert_eq!(
        card_lines, 1,
        "exactly one collapsed tool card line (not two); lines: {texts:?}"
    );
}

// Bug 2: a *failed* external tool call must keep its error detail — only the
// redundant `↳ ok · …` echo is noise; `↳ error · …` carries information.
#[test]
fn external_tool_failure_keeps_detail_line() {
    let mut state = make_state("ext-tool-fail");
    state.assistant_open = true;
    let mut save = None;

    apply_stream_event(
        &mut state,
        StreamEvent::ToolCall {
            name: "shell".into(),
            input: "cat missing".into(),
            full: Some("cat missing".into()),
        },
        &mut save,
    );
    apply_stream_event(
        &mut state,
        StreamEvent::Delta {
            text: "\n\u{21b3} error \u{00b7} no such file or directory\n".into(),
        },
        &mut save,
    );

    let texts = state
        .main_panel
        .lines_text(0, state.main_panel.len().saturating_sub(1));
    assert!(
        texts.iter().any(|t| t.contains("\u{21b3} error")),
        "failure detail must survive; lines: {texts:?}"
    );
}

// push_delta accumulated via the panel renders into the frame buffer.
#[test]
fn push_delta_accumulates_content_in_panel() {
    let mut state = make_state("sess-stream");
    state.main_panel.push_delta("hello");
    state.main_panel.push_delta(" there");
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("hello"),
        "delta content should appear in rendered buffer"
    );
}

// --- connect banner tests ---

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
    let resp_tier: Option<String> = resp.get("tier").and_then(|v| v.as_str()).map(str::to_owned);
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
fn push_message_trim_shifts_display_start_idx() {
    // `display_start_idx` is an absolute index into the bounded `messages` log.
    // When the ring trims its oldest entries on push, the watermark must shift by
    // the same amount so it keeps pointing at the same logical message rather than
    // drifting forward into live content.
    let mut state = make_state("sess-trim");
    // Fill to the cap so the next pushes trim from the front.
    for i in 0..MESSAGE_HISTORY_CAP {
        state.messages.push(Message {
            role: Role::System,
            text: format!("m{i}"),
        });
    }
    // Watermark sits in the middle of the buffer.
    state.display_start_idx = 100;
    let first_before = state.messages.first().map(|m| m.text.clone());

    // Push three more: each trims exactly one oldest entry.
    for j in 0..3 {
        state.push_message(Message {
            role: Role::System,
            text: format!("new{j}"),
        });
    }

    assert_eq!(
        state.messages.len(),
        MESSAGE_HISTORY_CAP,
        "length stays capped"
    );
    assert_eq!(
        state.display_start_idx, 97,
        "watermark shifts back by the number trimmed (3)"
    );
    assert_ne!(
        state.messages.first().map(|m| m.text.clone()),
        first_before,
        "oldest entries were actually trimmed"
    );
    assert_eq!(
        state.messages.last().map(|m| m.text.as_str()),
        Some("new2"),
        "newest entry retained at the tail"
    );
}

#[test]
fn push_message_trim_watermark_saturates_at_zero() {
    // A watermark already near the front must not underflow when more entries are
    // trimmed than lie before it — it saturates at 0 (show everything).
    let mut state = make_state("sess-trim-zero");
    for i in 0..MESSAGE_HISTORY_CAP {
        state.messages.push(Message {
            role: Role::System,
            text: format!("m{i}"),
        });
    }
    state.display_start_idx = 1;
    for j in 0..5 {
        state.push_message(Message {
            role: Role::System,
            text: format!("n{j}"),
        });
    }
    assert_eq!(state.display_start_idx, 0, "watermark saturates at zero");
}

#[test]
fn clear_command_advances_display_start() {
    let mut state = make_state("sess-clear");
    state.main_panel.push_line("old line 1".into());
    state.main_panel.push_line("old line 2".into());
    state.messages.push(Message {
        role: Role::System,
        text: "old line 1".into(),
    });
    state.messages.push(Message {
        role: Role::System,
        text: "old line 2".into(),
    });

    // Simulate /clear dispatch
    state.display_start_idx = state.messages.len();
    state.main_panel.clear_display();

    assert_eq!(state.display_start_idx, 2);
    assert_eq!(state.main_panel.display_start, 2);
    assert_eq!(state.main_panel.scroll, 2);
}

#[test]
fn new_lines_after_clear_are_visible() {
    let mut state = make_state("sess-clear2");
    state.main_panel.push_line("before clear".into());
    state.main_panel.clear_display();
    state.main_panel.push_line("after clear".into());
    // After clear, display_start=1, scroll=1; new line at index 1 is visible
    let visible = state.main_panel.lines_text(
        state.main_panel.display_start,
        state.main_panel.len().saturating_sub(1),
    );
    assert!(visible.iter().any(|l| l.contains("after clear")));
    assert!(!visible.iter().any(|l| l.contains("before clear")));
}

#[test]
fn push_system_message_routes_single_line_to_action_log() {
    let mut state = make_state("sess-emit");
    let log_before = state.action_log.len();
    push_system_message(&mut state, "diagram saved: ./out.svg");
    assert_eq!(
        state.action_log.len(),
        log_before + 1,
        "single-line system message must be added to action_log"
    );
}

#[test]
fn push_system_message_multi_line_stays_in_panel_only() {
    let mut state = make_state("sess-emit-multi");
    let log_before = state.action_log.len();
    push_system_message(&mut state, "line one\nline two\nline three");
    assert_eq!(
        state.action_log.len(),
        log_before,
        "multi-line system message must NOT be added to action_log"
    );
}

// --- prompt feedback: token estimate -------------------------------------

#[test]
fn active_agent_name_captured_from_stream_started_event() {
    let mut state = make_state("sess-agent");
    let event = serde_json::json!({"type": "started", "agent_name": "review"});
    if let Some(name) = event["agent_name"].as_str() {
        state.active_agent_name = Some(name.to_owned());
    }
    assert_eq!(state.active_agent_name.as_deref(), Some("review"));
}

// --- P4: PanelVisibility default ------------------------------------------

// --- quality gate: no synthetic cowork approval -----------------------------

fn quality_event(score: u8) -> StreamEvent {
    StreamEvent::Quality {
        score,
        tdd_pass: true,
        clean_pass: true,
        file_advisories: Vec::new(),
        skill_advisories: Vec::new(),
        llm_reviewed: false,
        suggested_command: None,
    }
}

#[test]
fn low_quality_streak_shows_notice_without_fake_cowork_item() {
    let mut state = make_state("sess-qgate");
    let mut save = None;
    apply_stream_event(&mut state, quality_event(50), &mut save);
    apply_stream_event(&mut state, quality_event(45), &mut save);

    assert!(
        state.pending_cowork.is_empty(),
        "no synthetic cowork item may be queued — approving it would target an id the daemon never registered"
    );
    let body = state
        .main_panel
        .lines_text(0, state.main_panel.len())
        .join("\n");
    assert!(
        body.contains("quality gate"),
        "informational notice must be shown; got: {body}"
    );
    assert!(
        body.contains("/quality"),
        "notice points at /quality: {body}"
    );
}

#[test]
fn quality_notice_fires_once_per_streak() {
    let mut state = make_state("sess-qgate-once");
    let mut save = None;
    apply_stream_event(&mut state, quality_event(50), &mut save);
    apply_stream_event(&mut state, quality_event(45), &mut save);
    apply_stream_event(&mut state, quality_event(40), &mut save);
    let body = state
        .main_panel
        .lines_text(0, state.main_panel.len())
        .join("\n");
    assert_eq!(
        body.matches("quality gate").count(),
        1,
        "notice fires once per streak, not per turn: {body}"
    );
}

// --- cowork wire extras (agent / cwd / risk) --------------------------------

#[test]
fn cowork_request_is_enriched_with_wire_extras() {
    let mut state = make_state("sess-cowork-meta");
    let mut save = None;
    let inbound = crate::events::InboundStreamEvent {
        event: StreamEvent::CoworkRequest {
            approval_id: "ap-1".into(),
            tool: "bash".into(),
            step_n: 2,
            args_display: r#"{"cmd":"ls"}"#.into(),
            reasoning: "check files".into(),
        },
        cowork_meta: Some(crate::events::CoworkMeta {
            agent: Some("review".into()),
            cwd: Some("/repo".into()),
            risk: Some("high".into()),
            supports_modify: None,
        }),
        cowork_resolved: None,
    };
    crate::events::apply_inbound_event(&mut state, inbound, &mut save);
    let item = state.pending_cowork.first().expect("cowork item queued");
    assert_eq!(item.agent.as_deref(), Some("review"));
    assert_eq!(item.cwd.as_deref(), Some("/repo"));
    assert_eq!(item.risk.as_deref(), Some("high"));
}

#[test]
fn cowork_request_without_extras_stays_absent() {
    let mut state = make_state("sess-cowork-plain");
    let mut save = None;
    let inbound = crate::events::InboundStreamEvent {
        event: StreamEvent::CoworkRequest {
            approval_id: "ap-2".into(),
            tool: "read".into(),
            step_n: 1,
            args_display: "{}".into(),
            reasoning: String::new(),
        },
        cowork_meta: None,
        cowork_resolved: None,
    };
    crate::events::apply_inbound_event(&mut state, inbound, &mut save);
    let item = state.pending_cowork.first().expect("cowork item queued");
    assert!(item.agent.is_none() && item.cwd.is_none() && item.risk.is_none());
}

// --- cowork ingest: sanitization, queue cap, and resolved dismissal --------

fn cowork_request_event(id: &str, tool: &str, args: &str) -> StreamEvent {
    StreamEvent::CoworkRequest {
        approval_id: id.into(),
        tool: tool.into(),
        step_n: 1,
        args_display: args.into(),
        reasoning: String::new(),
    }
}

fn inbound(event: StreamEvent) -> crate::events::InboundStreamEvent {
    crate::events::InboundStreamEvent {
        event,
        cowork_meta: None,
        cowork_resolved: None,
    }
}

#[test]
fn cowork_request_sanitizes_control_characters_at_ingest() {
    let mut state = make_state("sess-cowork-sanitize");
    let mut save = None;
    // OSC 52 clipboard-write attempt embedded in the args display.
    let dirty = "{\u{1b}]52;c;Zm9v\u{7}\"cmd\":\"ls\"}";
    crate::events::apply_inbound_event(
        &mut state,
        inbound(cowork_request_event("ap-esc", "ba\u{1b}sh", dirty)),
        &mut save,
    );
    let item = state.pending_cowork.first().expect("cowork item queued");
    let rendered = format!("{} {}", item.tool, item.args_display);
    assert!(
        !rendered.contains('\u{1b}') && !rendered.contains('\u{7}'),
        "no control characters may survive ingestion: {rendered:?}"
    );
}

#[test]
fn cowork_pending_queue_is_capped_and_drops_oldest_with_notice() {
    let mut state = make_state("sess-cowork-cap");
    let mut save = None;
    for i in 0..40 {
        let id = format!("ap-{i}");
        crate::events::apply_inbound_event(
            &mut state,
            inbound(cowork_request_event(&id, "bash", "{}")),
            &mut save,
        );
    }
    assert_eq!(
        state.pending_cowork.len(),
        32,
        "queue must be capped at 32 items"
    );
    assert_eq!(
        state.pending_cowork.first().map(|i| i.id.as_str()),
        Some("ap-8"),
        "oldest items are dropped first"
    );
    let body = state
        .main_panel
        .lines_text(0, state.main_panel.len())
        .join("\n");
    assert!(
        body.contains("dropped the oldest"),
        "dropping must be surfaced: {body}"
    );
}

#[test]
fn cowork_resolved_dismisses_matching_popup() {
    let mut state = make_state("sess-cowork-resolved");
    let mut save = None;
    crate::events::apply_inbound_event(
        &mut state,
        inbound(cowork_request_event("ap-x", "bash", "{}")),
        &mut save,
    );
    crate::events::apply_inbound_event(
        &mut state,
        inbound(cowork_request_event("ap-y", "read", "{}")),
        &mut save,
    );
    assert_eq!(state.pending_cowork.len(), 2);

    // Denied elsewhere: item dismissed, brief notice shown.
    let mut notice = inbound(StreamEvent::Unknown);
    notice.cowork_resolved = Some((
        "ap-x".to_owned(),
        Some(smedja_bellows::CoworkOutcome::Denied),
    ));
    crate::events::apply_inbound_event(&mut state, notice, &mut save);
    assert_eq!(state.pending_cowork.len(), 1);
    assert_eq!(state.pending_cowork[0].id, "ap-y");
    let body = state
        .main_panel
        .lines_text(0, state.main_panel.len())
        .join("\n");
    assert!(
        body.contains("approval resolved elsewhere: denied"),
        "denied-elsewhere notice; got: {body}"
    );

    // Approved elsewhere: dismissed silently (no second notice).
    let before = state.main_panel.len();
    let mut notice = inbound(StreamEvent::Unknown);
    notice.cowork_resolved = Some((
        "ap-y".to_owned(),
        Some(smedja_bellows::CoworkOutcome::Approved),
    ));
    crate::events::apply_inbound_event(&mut state, notice, &mut save);
    assert!(state.pending_cowork.is_empty());
    assert_eq!(
        state.main_panel.len(),
        before,
        "approved resolutions stay silent"
    );
}

#[test]
fn cowork_resolved_unknown_id_and_timeout_are_safe() {
    let mut state = make_state("sess-cowork-resolved-unknown");
    let mut save = None;
    crate::events::apply_inbound_event(
        &mut state,
        inbound(cowork_request_event("ap-z", "bash", "{}")),
        &mut save,
    );
    // Unknown id: no-op, no notice.
    let mut notice = inbound(StreamEvent::Unknown);
    notice.cowork_resolved = Some((
        "ap-other".to_owned(),
        Some(smedja_bellows::CoworkOutcome::Denied),
    ));
    crate::events::apply_inbound_event(&mut state, notice, &mut save);
    assert_eq!(state.pending_cowork.len(), 1);

    // Timeout of the known id dismisses with a notice.
    let mut notice = inbound(StreamEvent::Unknown);
    notice.cowork_resolved = Some((
        "ap-z".to_owned(),
        Some(smedja_bellows::CoworkOutcome::Timeout),
    ));
    crate::events::apply_inbound_event(&mut state, notice, &mut save);
    assert!(state.pending_cowork.is_empty());
    let body = state
        .main_panel
        .lines_text(0, state.main_panel.len())
        .join("\n");
    assert!(
        body.contains("approval resolved elsewhere: timeout"),
        "timeout notice; got: {body}"
    );
}

#[test]
fn cowork_resolved_clears_modify_state_when_head_dismissed() {
    let mut state = make_state("sess-cowork-modify-clear");
    let mut save = None;
    crate::events::apply_inbound_event(
        &mut state,
        inbound(cowork_request_event("ap-head", "bash", "{}")),
        &mut save,
    );
    crate::events::apply_inbound_event(
        &mut state,
        inbound(cowork_request_event("ap-tail", "read", "{}")),
        &mut save,
    );
    // User is editing replacement args for the head item.
    state.cowork_modify_mode = true;
    state.cowork_modify_input = r#"{"cmd":"ls"}"#.to_owned();

    let mut notice = inbound(StreamEvent::Unknown);
    notice.cowork_resolved = Some((
        "ap-head".to_owned(),
        Some(smedja_bellows::CoworkOutcome::Timeout),
    ));
    crate::events::apply_inbound_event(&mut state, notice, &mut save);

    assert_eq!(state.pending_cowork.len(), 1);
    assert!(
        !state.cowork_modify_mode && state.cowork_modify_input.is_empty(),
        "dismissing the head item must clear the modify state it was bound to"
    );

    // Dismissing a non-head item must NOT disturb an open modify editor.
    state.cowork_modify_mode = true;
    state.cowork_modify_input = "x".to_owned();
    let mut notice = inbound(StreamEvent::Unknown);
    notice.cowork_resolved = Some((
        "ap-tail".to_owned(),
        Some(smedja_bellows::CoworkOutcome::Denied),
    ));
    crate::events::apply_inbound_event(&mut state, notice, &mut save);
    assert!(state.pending_cowork.is_empty());
    // (head == tail after the first dismissal, so this also clears)
}

#[test]
fn cowork_resolved_without_outcome_dismisses_silently() {
    let mut state = make_state("sess-cowork-no-outcome");
    let mut save = None;
    crate::events::apply_inbound_event(
        &mut state,
        inbound(cowork_request_event("ap-silent", "bash", "{}")),
        &mut save,
    );
    let before = state.main_panel.len();
    // A line without a parseable outcome (older/never daemon) dismisses
    // without a blank "resolved elsewhere: " notice.
    let mut notice = inbound(StreamEvent::Unknown);
    notice.cowork_resolved = Some(("ap-silent".to_owned(), None));
    crate::events::apply_inbound_event(&mut state, notice, &mut save);
    assert!(state.pending_cowork.is_empty(), "item dismissed");
    assert_eq!(
        state.main_panel.len(),
        before,
        "no notice for a missing/unknown outcome"
    );
}

#[test]
fn cowork_request_applies_supports_modify_wire_flag() {
    let mut state = make_state("sess-cowork-supports-modify");
    let mut save = None;
    let inbound = crate::events::InboundStreamEvent {
        event: StreamEvent::CoworkRequest {
            approval_id: "ap-sm".into(),
            tool: "bash".into(),
            step_n: 1,
            args_display: "{}".into(),
            reasoning: String::new(),
        },
        cowork_meta: Some(crate::events::CoworkMeta {
            agent: None,
            cwd: None,
            risk: None,
            supports_modify: Some(false),
        }),
        cowork_resolved: None,
    };
    crate::events::apply_inbound_event(&mut state, inbound, &mut save);
    let item = state.pending_cowork.first().expect("cowork item queued");
    assert!(
        !item.supports_modify,
        "wire supports_modify:false must reach the item"
    );
}
