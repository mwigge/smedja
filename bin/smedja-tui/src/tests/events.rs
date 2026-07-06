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
