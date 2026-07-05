use super::*;

use std::path::PathBuf;
use uuid::Uuid;

// ── Phase 2 (retained) ────────────────────────────────────────────────

#[test]
fn agent_session_accumulates_lines() {
    let mut s = AgentSession::new("block1", "claude-opus");
    s.push_chunk(&AgentChunk {
        block_id: "block1".into(),
        text: "hello\nworld".into(),
        done: false,
        approval_required: false,
    });
    assert_eq!(s.content_lines(), vec!["hello", "world"]);
}

#[test]
fn agent_session_done_stops_streaming() {
    let mut s = AgentSession::new("block1", "claude-opus");
    s.push_chunk(&AgentChunk {
        block_id: "block1".into(),
        text: "done".into(),
        done: true,
        approval_required: false,
    });
    assert!(!s.streaming);
}

#[test]
fn agent_session_approval_pending_on_tool_call() {
    let mut s = AgentSession::new("block1", "claude-opus");
    s.push_chunk(&AgentChunk {
        block_id: "block1".into(),
        text: String::new(),
        done: false,
        approval_required: true,
    });
    assert_eq!(s.approval, ApprovalState::Pending);
}

#[test]
fn agent_session_approve_changes_state() {
    let mut s = AgentSession::new("b", "m");
    s.approval = ApprovalState::Pending;
    s.approve();
    assert_eq!(s.approval, ApprovalState::Approved);
}

#[test]
fn agent_session_deny_changes_state() {
    let mut s = AgentSession::new("b", "m");
    s.approval = ApprovalState::Pending;
    s.deny();
    assert_eq!(s.approval, ApprovalState::Denied);
}

#[test]
fn agent_session_respects_max_lines() {
    let mut s = AgentSession::new("b", "m");
    s.max_lines = 3;
    for i in 0..5 {
        s.push_chunk(&AgentChunk {
            block_id: "b".into(),
            text: format!("line{i}"),
            done: false,
            approval_required: false,
        });
    }
    assert!(s.lines.len() <= 3);
}

#[test]
fn agent_manager_creates_and_returns_session() {
    let mut mgr = AgentManager::new();
    let s = mgr.session_mut("b1", "model");
    s.push_chunk(&AgentChunk {
        block_id: "b1".into(),
        text: "hi".into(),
        done: false,
        approval_required: false,
    });
    assert_eq!(mgr.len(), 1);
    assert!(!mgr.is_empty());
}

#[test]
fn agent_manager_remove_returns_session() {
    let mut mgr = AgentManager::new();
    mgr.session_mut("b1", "m");
    let s = mgr.remove("b1");
    assert!(s.is_some());
    assert!(mgr.is_empty());
}

#[test]
fn shared_manager_is_clone() {
    let m = SharedAgentManager::new();
    let _m2 = m.clone();
}

// ── Phase 5 ───────────────────────────────────────────────────────────

#[test]
fn smdjad_socket_path_uses_xdg_runtime_dir() {
    // Temporarily set XDG_RUNTIME_DIR; restore afterward.
    let _guard = EnvGuard::set("XDG_RUNTIME_DIR", "/run/user/1000");
    let path = smdjad_socket_path();
    assert_eq!(path, PathBuf::from("/run/user/1000/smdjad.sock"));
}

#[test]
fn socket_path_matches_smdjad() {
    // st-agent and smdjad must agree on the socket path for a given XDG_RUNTIME_DIR.
    // This test verifies the st-agent path matches the expected format.
    let _guard = EnvGuard::set("XDG_RUNTIME_DIR", "/run/user/9999");
    let path = smdjad_socket_path();
    assert_eq!(
        path.to_str().unwrap(),
        "/run/user/9999/smdjad.sock",
        "socket path must be $XDG_RUNTIME_DIR/smdjad.sock"
    );
    // Confirm no subdirectory: path should not contain /smedja/
    assert!(
        !path.to_str().unwrap().contains("/smedja/"),
        "socket path must not contain /smedja/ subdirectory"
    );
}

#[test]
fn smdjad_socket_path_falls_back_to_tmp() {
    let _guard = EnvGuard::remove("XDG_RUNTIME_DIR");
    let path = smdjad_socket_path();
    assert_eq!(path, PathBuf::from("/tmp/smdjad.sock"));
}

fn envelope_line(event: smedja_agent_events::AgentEvent) -> String {
    smedja_agent_events::AgentEventEnvelope::new(event).to_json_line()
}

#[test]
fn pane_event_deserialise_turn_start() {
    let line = envelope_line(smedja_agent_events::AgentEvent::TurnStart {
        turn_id: Some("t1".into()),
        session_id: Some("s1".into()),
    });
    let event = PaneEvent::from_json_line(&line).expect("should parse");
    if let PaneEvent::TurnStart {
        session_id,
        turn_id,
        tier,
        model,
        ..
    } = event
    {
        assert_eq!(session_id, "s1");
        assert_eq!(turn_id, "t1");
        // The wire schema does not carry tier/model; they default to empty.
        assert_eq!(tier, "");
        assert_eq!(model, "");
    } else {
        panic!("wrong variant");
    }
}

#[test]
fn pane_event_deserialise_tool_call() {
    let line = envelope_line(smedja_agent_events::AgentEvent::ToolCall {
        turn_id: Some("t1".into()),
        tool: Some("bash".into()),
        summary: Some("ls -la".into()),
    });
    let event = PaneEvent::from_json_line(&line).expect("should parse");
    if let PaneEvent::ToolCall {
        tool_name,
        args_summary,
        ..
    } = event
    {
        assert_eq!(tool_name, "bash");
        assert_eq!(args_summary, "ls -la");
    } else {
        panic!("wrong variant");
    }
}

#[test]
fn pane_event_deserialise_approval_prompt() {
    let line = envelope_line(smedja_agent_events::AgentEvent::ApprovalPrompt {
        turn_id: Some("t1".into()),
        tool: Some("rm".into()),
        prompt: Some("Allow deletion?".into()),
    });
    let event = PaneEvent::from_json_line(&line).expect("should parse");
    if let PaneEvent::ApprovalPrompt {
        tool_name, prompt, ..
    } = event
    {
        assert_eq!(tool_name, "rm");
        assert_eq!(prompt, "Allow deletion?");
    } else {
        panic!("wrong variant");
    }
}

#[test]
fn pane_event_deserialise_tool_result() {
    let line = envelope_line(smedja_agent_events::AgentEvent::ToolResult {
        turn_id: Some("t1".into()),
        tool: Some("read".into()),
        summary: Some("12 lines".into()),
        ok: Some(true),
    });
    let event = PaneEvent::from_json_line(&line).expect("should parse");
    if let PaneEvent::ToolResult { tool_name, outcome } = event {
        assert_eq!(tool_name, "read");
        assert_eq!(outcome, "12 lines");
    } else {
        panic!("wrong variant");
    }
}

#[test]
fn pane_event_deserialise_turn_end() {
    let line = envelope_line(smedja_agent_events::AgentEvent::TurnEnd {
        turn_id: Some("t1".into()),
        session_id: Some("s1".into()),
        tokens_saved: None,
        efficiency_ratio: None,
        input_tokens: None,
        output_tokens: None,
        latency_ms: None,
        traceparent: None,
    });
    let event = PaneEvent::from_json_line(&line).expect("should parse");
    assert!(matches!(event, PaneEvent::TurnEnd { .. }));
}

#[test]
fn pane_event_turn_end_carries_real_token_latency_trace() {
    // The v3 wire fields must survive the envelope → PaneEvent mapping with
    // their exact values, not the historical 0/None placeholders.
    let line = envelope_line(smedja_agent_events::AgentEvent::TurnEnd {
        turn_id: Some("t1".into()),
        session_id: Some("s1".into()),
        tokens_saved: None,
        efficiency_ratio: None,
        input_tokens: Some(412),
        output_tokens: Some(88),
        latency_ms: Some(4200),
        traceparent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into()),
    });
    let event = PaneEvent::from_json_line(&line).expect("should parse");
    match event {
        PaneEvent::TurnEnd {
            input_tokens,
            output_tokens,
            latency_ms,
            traceparent,
            ..
        } => {
            assert_eq!(input_tokens, 412);
            assert_eq!(output_tokens, 88);
            assert_eq!(latency_ms, 4200);
            assert_eq!(
                traceparent.as_deref(),
                Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
            );
        }
        other => panic!("expected TurnEnd, got {other:?}"),
    }
}

#[test]
fn pane_event_turn_end_carries_savings_figure() {
    let line = envelope_line(smedja_agent_events::AgentEvent::TurnEnd {
        turn_id: Some("t1".into()),
        session_id: Some("s1".into()),
        tokens_saved: Some(5000),
        efficiency_ratio: Some(0.3),
        input_tokens: None,
        output_tokens: None,
        latency_ms: None,
        traceparent: None,
    });
    let event = PaneEvent::from_json_line(&line).expect("should parse");
    match event {
        PaneEvent::TurnEnd {
            tokens_saved,
            efficiency_ratio,
            ..
        } => {
            assert_eq!(tokens_saved, Some(5000));
            assert_eq!(efficiency_ratio, Some(0.3));
        }
        other => panic!("expected TurnEnd, got {other:?}"),
    }
}

#[test]
fn apply_turn_end_accumulates_savings_into_state() {
    let mut state = PaneAgentState::default();
    state.apply_turn_end(&PaneEvent::TurnEnd {
        input_tokens: 10,
        output_tokens: 5,
        latency_ms: 100,
        traceparent: None,
        tokens_saved: Some(4242),
        efficiency_ratio: Some(0.41),
    });
    assert_eq!(state.last_input_tokens, Some(10));
    assert_eq!(state.tokens_saved, Some(4242));
    assert_eq!(state.efficiency_ratio, Some(0.41));
}

#[test]
fn apply_turn_end_keeps_prior_savings_when_absent() {
    let mut state = PaneAgentState {
        tokens_saved: Some(100),
        efficiency_ratio: Some(0.5),
        ..PaneAgentState::default()
    };
    // A later TurnEnd that does not report savings must not clobber the
    // accumulated figure with None (no misleading reset to zero).
    state.apply_turn_end(&PaneEvent::TurnEnd {
        input_tokens: 1,
        output_tokens: 1,
        latency_ms: 1,
        traceparent: None,
        tokens_saved: None,
        efficiency_ratio: None,
    });
    assert_eq!(state.tokens_saved, Some(100));
    assert_eq!(state.efficiency_ratio, Some(0.5));
}

#[test]
fn pane_event_deserialise_stream_delta() {
    let line = envelope_line(smedja_agent_events::AgentEvent::StreamDelta {
        turn_id: Some("t1".into()),
        content: Some("partial".into()),
    });
    let event = PaneEvent::from_json_line(&line).expect("should parse");
    if let PaneEvent::StreamDelta { text } = event {
        assert_eq!(text, "partial");
    } else {
        panic!("wrong variant");
    }
}

#[test]
fn pane_event_unknown_type_returns_none() {
    assert!(PaneEvent::from_json_line(r#"{"type":"nope"}"#).is_none());
    assert!(PaneEvent::from_json_line("not json").is_none());
}

#[test]
fn approval_gate_render_lines_contains_tool_name() {
    let gate = ApprovalGate {
        pane_id: "pane-1".into(),
        tool_name: "bash".into(),
        args: serde_json::json!({"cmd": "ls"}),
        prompt: "Allow bash?".into(),
        state: ApprovalState::Pending,
    };
    let lines = gate.render_lines();
    assert!(
        lines.iter().any(|l| l.contains("bash")),
        "render_lines must mention the tool name"
    );
}

#[test]
fn pane_env_var_returns_correct_key() {
    let id = Uuid::new_v4();
    let (key, val) = pane_env_var(&id);
    assert_eq!(key, "SMEDJA_TERM_PANE");
    assert_eq!(val, id.to_string());
}

#[test]
fn shared_pane_state_is_clone() {
    let s = SharedPaneState::new();
    let _s2 = s.clone();
}

#[test]
fn agent_session_suppress_flag_defaults_false() {
    let s = AgentSession::new("b", "m");
    assert!(!s.suppress_pty_output);
}

/// A legacy payload lacking a `schema_version` field still decodes via the
/// envelope's `#[serde(default)]` version handling, and maps to the right
/// variant — exercising backward compatibility on the receive path.
#[test]
fn pane_event_decodes_legacy_versionless_line() {
    let line = r#"{"type":"turn_start","turn_id":"t0","session_id":"old"}"#;
    let event = PaneEvent::from_json_line(line).expect("legacy line must decode");
    if let PaneEvent::TurnStart {
        session_id,
        turn_id,
        ..
    } = event
    {
        assert_eq!(session_id, "old");
        assert_eq!(turn_id, "t0");
    } else {
        panic!("expected TurnStart");
    }
}

// ── Test helpers ─────────────────────────────────────────────────────

/// RAII guard that sets or removes an environment variable and restores the
/// original value on drop.  Using this avoids cross-test pollution when
/// tests run in the same process.
struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    key: String,
    previous: Option<String>,
}

fn env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

impl EnvGuard {
    fn set(key: &str, value: &str) -> Self {
        let lock = env_lock().lock().expect("env test lock poisoned");
        let previous = std::env::var(key).ok();
        // SAFETY: environment-mutating tests are serialised by env_lock and
        // the original value is restored while the lock is still held.
        unsafe { std::env::set_var(key, value) };
        Self {
            _lock: lock,
            key: key.to_owned(),
            previous,
        }
    }

    fn remove(key: &str) -> Self {
        let lock = env_lock().lock().expect("env test lock poisoned");
        let previous = std::env::var(key).ok();
        // SAFETY: environment-mutating tests are serialised by env_lock and
        // the original value is restored while the lock is still held.
        unsafe { std::env::remove_var(key) };
        Self {
            _lock: lock,
            key: key.to_owned(),
            previous,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(v) => unsafe { std::env::set_var(&self.key, v) },
            None => unsafe { std::env::remove_var(&self.key) },
        }
    }
}

#[test]
fn agent_socket_path_appends_dot_agent() {
    let p = agent_socket_path(std::path::Path::new("/run/smdjad.sock"));
    assert_eq!(
        p,
        std::path::PathBuf::from("/run/smdjad.sock.agent"),
        "expected .agent suffix"
    );
}

#[test]
fn pane_agent_state_has_new_token_fields() {
    let mut state = PaneAgentState::default();
    assert!(state.last_input_tokens.is_none());
    assert!(state.last_output_tokens.is_none());
    assert!(state.last_latency_ms.is_none());
    assert!(state.last_traceparent.is_none());
    state.last_input_tokens = Some(412);
    state.last_output_tokens = Some(88);
    state.last_latency_ms = Some(4200);
    state.last_traceparent = Some("00-abc-01".to_owned());
    assert_eq!(state.last_input_tokens, Some(412));
    assert_eq!(state.last_output_tokens, Some(88));
}
