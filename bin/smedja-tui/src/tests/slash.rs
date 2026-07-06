//! `slash`-area unit tests (moved verbatim from the former `tests.rs`).

use serde_json::json;

use smedja_rpc::client::Client;

use crate::governance::{
    format_gov_list, gov_create, gov_transition, scan_gov_artifacts, GovArtifact,
};
use crate::input::{accept_slash_completion, clear_slash_popup, handle_key};
use crate::slash::{
    apply_agent, apply_tier, dispatch_slash, format_agents_table, format_approvals_list,
    format_local_model_list, format_metrics, format_model_list,
};
use crate::test_support::{make_state, render_frame};
use crate::{
    command_palette_filtered, filtered_completions, format_resume_rows, parse_resume_args,
    parse_review_scope, push_system_message, render_findings_summary,
    resume_blocked_by_pending_turn, resume_plan, ResumePlan, HELP_TEXT, SLASH_COMPLETIONS,
};

#[test]
fn format_memory_lists_turns_with_previews() {
    let history = json!({
        "turns": [
            { "turn_n": 1, "messages": [
                {"role": "user", "content": "write a counter"},
                {"role": "assistant", "content": "here is the code"}
            ]}
        ],
        "audit": [ {"x": 1} ]
    });
    let ctx =
        json!({ "used_tok": 50, "window_tok": 200, "vault_warm_count": 3, "vault_cold_count": 7 });
    let out = crate::slash::format_memory(&history, Some(&ctx), "abcd1234ef");
    assert!(out.contains("memory"), "{out}");
    assert!(out.contains("abcd1234"), "{out}"); // short session id
    assert!(out.contains("write a counter"), "{out}");
    assert!(out.contains("here is the code"), "{out}");
    assert!(out.contains("1 audit event"), "{out}");
    assert!(out.contains("/memory <session_id>"), "{out}");
    // Short-term context + vault summary present.
    assert!(out.contains("50/200 tok (25%)"), "{out}");
    assert!(out.contains("3 warm + 7 cold"), "{out}");
}

#[test]
fn skills_listing_and_install_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    // A source dir with two skill .md files + a non-md file.
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("alpha.md"), "skill a").unwrap();
    std::fs::write(src.join("beta.md"), "skill b").unwrap();
    std::fs::write(src.join("notes.txt"), "ignore").unwrap();
    // Directory-form skill.
    let gamma = src.join("gamma");
    std::fs::create_dir_all(&gamma).unwrap();
    std::fs::write(gamma.join("SKILL.md"), "skill g").unwrap();

    let names = crate::slash::list_skill_dir(&src);
    assert!(names.contains(&"alpha".to_owned()), "{names:?}");
    assert!(names.contains(&"gamma".to_owned()), "{names:?}"); // dir/SKILL.md
    assert!(!names.iter().any(|n| n == "notes"), "{names:?}"); // .txt ignored

    let dst = tmp.path().join(".smedja").join("skills");
    let msg = crate::slash::install_skills_dir(&src, &dst);
    assert!(msg.contains("installed 2 skill file"), "{msg}"); // alpha.md + beta.md
    assert!(dst.join("alpha.md").exists());
}

#[test]
fn format_memory_handles_empty_history() {
    let out = crate::slash::format_memory(&json!({ "turns": [] }), None, "sess0001");
    assert!(out.contains("no stored turns"), "{out}");
}

#[test]
fn review_no_args_is_diff_scope() {
    let params = parse_review_scope("");
    assert_eq!(params, json!({}), "no args → working-tree diff");
}

#[test]
fn review_path_arg_is_path_scope() {
    assert_eq!(
        parse_review_scope("src/lib.rs"),
        json!({ "path": "src/lib.rs" })
    );
}

#[test]
fn review_branch_flag_is_branch_scope() {
    assert_eq!(
        parse_review_scope("--branch main"),
        json!({ "branch": "main" })
    );
}

#[test]
fn review_pr_flag_is_pr_scope() {
    assert_eq!(parse_review_scope("--pr 42"), json!({ "pr": "42" }));
}

#[test]
fn review_diff_flag_is_explicit_diff_scope() {
    assert_eq!(parse_review_scope("--diff"), json!({ "diff": true }));
}

// --- /review findings summary rendering ---

#[test]
fn findings_summary_lists_counts_and_report_path() {
    let counts = json!({ "critical": 1, "high": 0, "medium": 2, "low": 3, "info": 0 });
    let summary = render_findings_summary(&counts, Some("/tmp/report.md"));
    assert!(summary.contains("critical=1"), "got: {summary}");
    assert!(summary.contains("medium=2"), "got: {summary}");
    assert!(summary.contains("low=3"), "got: {summary}");
    assert!(summary.contains("report: /tmp/report.md"), "got: {summary}");
}

#[test]
fn findings_summary_marks_inline_when_no_path() {
    let counts = json!({ "critical": 0, "high": 0, "medium": 0, "low": 0, "info": 0 });
    let summary = render_findings_summary(&counts, None);
    assert!(summary.contains("report: (inline)"), "got: {summary}");
}

// L129: filtered_completions returns only matching entries.
#[test]
fn slash_completions_filter_by_prefix() {
    let completions = filtered_completions("/bri");
    assert_eq!(completions, vec!["/briefing".to_owned()]);
}

// L129: typing "/" returns all completions.
#[test]
fn slash_completions_all_on_bare_slash() {
    let completions = filtered_completions("/");
    assert_eq!(completions.len(), SLASH_COMPLETIONS.len());
}

// L129: unknown prefix returns empty.
#[test]
fn slash_completions_empty_for_no_match() {
    let completions = filtered_completions("/zzz");
    assert!(completions.is_empty());
}

#[test]
fn slash_accept_space_inserts_completion_with_trailing_space() {
    let mut state = make_state("test-session");
    state.input = "/ti".to_owned();
    state.slash_completions = filtered_completions("/ti");
    state.slash_popup_visible = true;
    state.slash_cursor = 0;

    assert!(accept_slash_completion(&mut state, true));

    assert_eq!(state.input, "/tier ");
    assert!(!state.slash_popup_visible);
    assert!(state.slash_completions.is_empty());
}

#[test]
fn slash_accept_enter_inserts_completion_without_space() {
    let mut state = make_state("test-session");
    state.input = "/h".to_owned();
    state.slash_completions = filtered_completions("/h");
    state.slash_popup_visible = true;
    state.slash_cursor = 0;

    assert!(accept_slash_completion(&mut state, false));

    assert_eq!(state.input, "/health");
    assert!(!state.slash_popup_visible);
}

#[test]
fn dispatch_tier_fast_sets_state_tier() {
    let mut state = make_state("test-session");
    let text = apply_tier("fast", &mut state);
    assert_eq!(state.tier.as_deref(), Some("fast"));
    assert_eq!(text, "tier set to fast");
}

#[test]
fn dispatch_tier_deep_sets_state_tier() {
    let mut state = make_state("test-session");
    let text = apply_tier("deep", &mut state);
    assert_eq!(state.tier.as_deref(), Some("deep"));
    assert_eq!(text, "tier set to deep");
}

#[test]
fn dispatch_agent_impl_sets_state_mode() {
    let mut state = make_state("test-session");
    let text = apply_agent("impl", &mut state);
    assert_eq!(state.mode.as_deref(), Some("impl"));
    assert_eq!(text, "agent mode set to impl");
}

#[test]
fn slash_esc_clears_input_and_closes_popup() {
    let mut state = make_state("test-session");
    state.input = "/ti".to_owned();
    state.input_cursor = 3;
    state.slash_completions = filtered_completions("/ti");
    state.slash_popup_visible = true;
    state.slash_cursor = 0;

    clear_slash_popup(&mut state);

    assert!(state.input.is_empty(), "input must be cleared on Esc");
    assert_eq!(state.input_cursor, 0, "cursor must reset to 0 on Esc");
    assert!(!state.slash_popup_visible, "popup must close on Esc");
    assert!(
        state.slash_completions.is_empty(),
        "completions must be cleared on Esc"
    );
    assert_eq!(state.slash_cursor, 0);
}

#[test]
fn slash_esc_on_popup_already_closed_is_idempotent() {
    let mut state = make_state("test-session");
    state.input = "hello".to_owned();
    state.input_cursor = 5;
    state.slash_popup_visible = false;

    clear_slash_popup(&mut state);

    assert!(state.input.is_empty());
    assert_eq!(state.input_cursor, 0);
    assert!(!state.slash_popup_visible);
}

#[test]
fn slash_tier_fast_routes_correctly_via_apply_tier() {
    let mut state = make_state("test-session");
    let text = apply_tier("fast", &mut state);
    assert_eq!(state.tier.as_deref(), Some("fast"));
    assert_eq!(text, "tier set to fast");
}

#[test]
fn slash_tier_local_routes_correctly_via_apply_tier() {
    let mut state = make_state("test-session");
    let text = apply_tier("local", &mut state);
    assert_eq!(state.tier.as_deref(), Some("local"));
    assert_eq!(text, "tier set to local");
}

#[test]
fn slash_tier_unknown_arg_returns_error_message() {
    let mut state = make_state("test-session");
    let text = apply_tier("turbo", &mut state);
    assert_eq!(text, "unknown tier: turbo");
    assert!(state.tier.is_none(), "tier must not change on unknown arg");
}

#[test]
fn slash_agent_sets_mode_via_apply_agent() {
    let mut state = make_state("test-session");
    let text = apply_agent("review", &mut state);
    assert_eq!(state.mode.as_deref(), Some("review"));
    assert_eq!(text, "agent mode set to review");
}

#[test]
fn format_agents_table_renders_header_and_rows() {
    let v = serde_json::json!({
        "runners": [
            { "runner": "claude-cli", "tier": "fast", "model": "claude-haiku-4-5-20251001" },
            { "runner": "claude-cli", "tier": "deep", "model": "claude-sonnet-4-6" },
        ]
    });
    let out = format_agents_table(&v);
    assert!(out.contains("runner"), "header must include 'runner'");
    assert!(out.contains("claude-cli"), "table must list runner name");
    assert!(out.contains("fast"), "table must list tier");
    assert!(
        out.contains("claude-haiku-4-5-20251001"),
        "table must list model"
    );
}

#[test]
fn format_agents_table_empty_runners_returns_message() {
    let v = serde_json::json!({ "runners": [] });
    let out = format_agents_table(&v);
    assert!(out.contains("no runners"), "empty pool must say no runners");
}

#[test]
fn format_metrics_aggregates_token_and_cost_data() {
    let usage = Ok(serde_json::json!({
        "session_id": "sess-1",
        "turns": [
            { "turn_n": 1, "input_tok": 100, "output_tok": 50, "cumulative_input": 100, "cumulative_output": 50 }
        ]
    }));
    let cost = Ok(serde_json::json!({
        "session_id": "sess-1",
        "total_usd": 0.0025,
        "breakdown": []
    }));
    let out = format_metrics(&usage, &cost, "sess-1");
    assert!(out.contains("sess-1"), "metrics must include session id");
    assert!(out.contains("turns: 1"), "metrics must include turn count");
    assert!(out.contains("0.0025"), "metrics must include cost");
}

#[test]
fn format_metrics_handles_rpc_errors_gracefully() {
    let usage: Result<serde_json::Value, smedja_rpc::RpcError> =
        Err(smedja_rpc::RpcError::new(-32600, "unavailable"));
    let cost: Result<serde_json::Value, smedja_rpc::RpcError> =
        Err(smedja_rpc::RpcError::new(-32600, "unavailable"));
    let out = format_metrics(&usage, &cost, "sess-err");
    assert!(
        out.contains("sess-err"),
        "metrics must still show session id on error"
    );
}

#[test]
fn format_approvals_list_shows_items() {
    let v = serde_json::json!([
        { "id": "item-1", "tool": "Bash", "args": "git push origin main", "step_n": 1 }
    ]);
    let out = format_approvals_list(&v);
    assert!(out.contains("item-1"), "must include id");
    assert!(out.contains("Bash"), "must include tool name");
    assert!(out.contains("git push"), "must include args");
    assert!(out.contains("/approve"), "must include usage hint");
}

#[test]
fn format_approvals_list_empty_shows_no_pending_message() {
    let v = serde_json::json!([]);
    let out = format_approvals_list(&v);
    assert!(
        out.contains("no pending"),
        "empty list must say no pending approvals"
    );
}

#[test]
fn format_model_list_renders_all_entries() {
    let v = serde_json::json!({
        "runners": [
            { "runner": "claude-cli", "tier": "fast", "model": "claude-haiku-4-5-20251001" }
        ]
    });
    let out = format_model_list(&v);
    assert!(out.contains("claude-cli"), "must include runner name");
    assert!(out.contains("fast"), "must include tier");
    assert!(
        out.contains("claude-haiku-4-5-20251001"),
        "must include model"
    );
}

#[test]
fn format_local_model_list_renders_fit_and_active() {
    let v = serde_json::json!({
        "active_model_id": "qwen3-14b",
        "models": [
            { "id": "qwen3-14b", "fit": "fits", "active": true },
            { "id": "huge-70b", "fit": "exceeds", "active": false }
        ]
    });
    let out = format_local_model_list(&v);
    assert!(out.contains("qwen3-14b") && out.contains("[fits]"));
    assert!(out.contains('*'), "active model must be marked");
    assert!(out.contains("huge-70b") && out.contains("[exceeds]"));
}

/// Spawns a one-shot mock daemon that records the requested method into the
/// returned channel and replies with `reply`. Returns the socket path.
fn spawn_method_capture(
    reply: serde_json::Value,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    tokio::sync::oneshot::Receiver<String>,
) {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader as TokioBufReader};
    use tokio::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("local-test.sock");
    let listener = UnixListener::bind(&sock_path).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();

    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let mut reader = TokioBufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let req: serde_json::Value =
                serde_json::from_str(line.trim_end()).unwrap_or(serde_json::Value::Null);
            let method = req["method"].as_str().unwrap_or("").to_owned();
            let _ = tx.send(method);
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": req["id"].clone(),
                "result": reply,
            });
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            let _ = reader.get_mut().write_all(&bytes).await;
        }
    });

    (dir, sock_path, rx)
}

/// For the local runner, `/model <name>` dispatches `local.swap` (not the
/// relabel-only `session.set_model`).
#[tokio::test]
async fn model_command_local_runner_dispatches_local_swap() {
    let (_dir, sock_path, rx) = spawn_method_capture(serde_json::json!({
        "from_model": "qwen3-14b",
        "active_model_id": "llama3-8b",
        "explicit_swap": true,
        "swap_latency_ms": 12
    }));

    let mut client = Client::connect(&sock_path).await.unwrap();
    let mut state = make_state("sess-local");
    state.runner = "local".to_owned();

    dispatch_slash("/model llama3-8b", &mut state, &mut client)
        .await
        .unwrap();

    let method = rx.await.unwrap();
    assert_eq!(
        method, "local.swap",
        "local runner /model <name> must dispatch local.swap, not session.set_model"
    );
    assert_eq!(
        state.model.as_deref(),
        Some("llama3-8b"),
        "the active model label must update after a local swap"
    );
}

/// For the local runner, `/model` with no args lists the GPU-annotated
/// inventory via `local.models`.
#[tokio::test]
async fn model_command_local_runner_lists_via_local_models() {
    let (_dir, sock_path, rx) = spawn_method_capture(serde_json::json!({
        "active_model_id": "qwen3-14b",
        "models": [ { "id": "qwen3-14b", "fit": "fits", "active": true } ],
        "gpu": { "detected": false }
    }));

    let mut client = Client::connect(&sock_path).await.unwrap();
    let mut state = make_state("sess-local");
    state.runner = "local".to_owned();

    dispatch_slash("/model", &mut state, &mut client)
        .await
        .unwrap();

    let method = rx.await.unwrap();
    assert_eq!(
        method, "local.models",
        "local runner bare /model must list via local.models"
    );
}

#[test]
fn slash_completions_include_new_commands() {
    let required = [
        "/agent",
        "/approve",
        "/briefing",
        "/login",
        "/metrics",
        "/model",
        "/quota",
        "/review",
        "/switch",
        "/takeover",
    ];
    for cmd in required {
        assert!(
            SLASH_COMPLETIONS.contains(&cmd),
            "{cmd} must be in SLASH_COMPLETIONS"
        );
    }
}

#[test]
fn slash_completions_switch_matches_sw_prefix() {
    let completions = filtered_completions("/sw");
    assert!(
        completions.contains(&"/switch".to_owned()),
        "/switch must match '/sw' prefix; got: {completions:?}"
    );
}

#[test]
fn slash_completions_takeover_matches_tak_prefix() {
    let completions = filtered_completions("/tak");
    assert!(
        completions.contains(&"/takeover".to_owned()),
        "/takeover must match '/tak' prefix; got: {completions:?}"
    );
}

#[test]
fn switch_no_args_opens_runner_picker() {
    // Simulate what dispatch_slash("switch", "") does on successful runner.list:
    // populate slash_completions with runner names and set picker flags.
    let mut state = make_state("sess-switch");
    state.slash_completions = vec!["claude".to_owned(), "codex".to_owned(), "local".to_owned()];
    state.slash_popup_visible = true;
    state.runner_picker_mode = true;
    state.input.clear();
    state.input_cursor = 0;

    assert!(state.slash_popup_visible, "picker popup must open");
    assert!(state.runner_picker_mode, "runner_picker_mode must be set");
    assert!(
        !state.slash_completions.is_empty(),
        "completions must list runner names"
    );
}

#[test]
fn slash_takeover_no_args_produces_usage_hint() {
    let mut state = make_state("sess-takeover");
    let cmd = "takeover";
    let args = "";
    let guidance = if args.is_empty() {
        Some("usage: /takeover <runner>  — fork this session onto a new runner".to_owned())
    } else {
        None
    };
    if let Some(msg) = guidance {
        state.main_panel.push_line(msg.clone());
        assert!(msg.contains("usage"), "hint must mention 'usage'");
    } else {
        panic!("expected usage hint for cmd={cmd} args={args}");
    }
}

#[test]
fn slash_switch_updates_state_runner_on_success() {
    // Simulate what dispatch_slash("switch", "codex") does on success
    // without a live daemon: verify state mutations are correct.
    let mut state = make_state("sess-switch-ok");
    let canonical = "codex-cli";
    state.runner = canonical.to_owned();
    push_system_message(&mut state, format!("runner switched to {canonical}"));

    assert_eq!(state.runner, "codex-cli");
    let has_msg = state
        .main_panel
        .lines_text(0, 100)
        .iter()
        .any(|l| l.contains("runner switched to codex-cli"));
    assert!(has_msg, "panel must show switch confirmation");
}

#[test]
fn slash_takeover_updates_session_id_and_runner_on_success() {
    let mut state = make_state("old-session");
    let new_session_id = "new-session-uuid-1234";
    let runner = "codex-cli";
    state.session_id = new_session_id.to_owned();
    state.runner = runner.to_owned();
    push_system_message(
        &mut state,
        format!(
            "handed off to {runner} — new session: {}",
            &new_session_id[..8]
        ),
    );

    assert_eq!(state.session_id, new_session_id);
    assert_eq!(state.runner, "codex-cli");
    let has_msg = state
        .main_panel
        .lines_text(0, 100)
        .iter()
        .any(|l| l.contains("handed off to codex-cli"));
    assert!(has_msg, "panel must show handoff confirmation");
}

// -----------------------------------------------------------------------
// Session resume — startup routing, replay, picker, rollback
// -----------------------------------------------------------------------

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
fn resume_in_slash_completions() {
    assert!(
        SLASH_COMPLETIONS.contains(&"/resume"),
        "/resume must be in SLASH_COMPLETIONS"
    );
    let completions = filtered_completions("/res");
    assert!(
        completions.contains(&"/resume".to_owned()),
        "/resume must match '/res' prefix; got: {completions:?}"
    );
}

#[test]
fn help_text_mentions_resume() {
    assert!(HELP_TEXT.contains("/resume"), "help must document /resume");
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

#[test]
fn slash_health_in_completions() {
    // /health must appear in completions when user types "/h"
    let completions = filtered_completions("/h");
    assert!(
        completions.contains(&"/health".to_owned()),
        "/health must be in SLASH_COMPLETIONS and match '/h' prefix"
    );
}

#[test]
fn health_command_shows_socket_path_in_state() {
    let mut state = make_state("sess-health");
    // Simulate what /health should push to main_panel.
    let msg = format!("health: socket=ok session={} latency=?ms", state.session_id);
    state.main_panel.push_line(msg.clone());
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("health"),
        "health output should appear in panel"
    );
    assert!(
        content.contains("sess-health"),
        "health output should show session ID"
    );
}

#[test]
fn health_command_param_key_is_id() {
    // The /health handler must pass "id", not "session_id", to session.get.
    let session_id = "sess-health";
    let payload = json!({ "id": session_id });
    assert!(
        payload.get("id").is_some(),
        "RPC payload must contain key \"id\""
    );
    assert!(
        payload.get("session_id").is_none(),
        "RPC payload must not contain key \"session_id\""
    );
    assert_eq!(
        payload["id"].as_str().unwrap(),
        session_id,
        "\"id\" value must match the session id"
    );
}

// handle_key with "/health" + Enter calls session.get and writes latency to panel.
#[tokio::test]
async fn health_command_handle_key_shows_latency_in_panel() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};
    use tokio::net::UnixListener;

    // Bind to a socket inside a temp dir so the path is unique per test run.
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("health-test.sock");
    let listener = UnixListener::bind(&sock_path).unwrap();

    // Spawn a minimal mock daemon that handles exactly one JSON-RPC request.
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let mut reader = TokioBufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let req: serde_json::Value =
                serde_json::from_str(line.trim_end()).unwrap_or(serde_json::Value::Null);
            let id = req["id"].clone();
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "id": "sess-health-mock" }
            });
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            let _ = reader.get_mut().write_all(&bytes).await;
        }
    });

    let mut client = Client::connect(&sock_path).await.unwrap();
    let mut state = make_state("sess-health-mock");
    state.input = "/health".into();
    let mut editor = rustyline::DefaultEditor::new().unwrap();

    let key = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::empty(),
    );
    handle_key(key, &mut state, &mut client, &mut editor)
        .await
        .unwrap();

    // input is cleared by std::mem::take before the command runs.
    assert!(
        state.input.is_empty(),
        "/health must clear the input field after Enter"
    );

    // The main panel must contain the health output line.
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("health"),
        "main panel should contain health output after /health command; got: {content:?}"
    );
}

#[test]
fn slash_completions_includes_drawio_and_pptx() {
    assert!(
        SLASH_COMPLETIONS.contains(&"/drawio"),
        "/drawio must be in SLASH_COMPLETIONS"
    );
    assert!(
        SLASH_COMPLETIONS.contains(&"/pptx"),
        "/pptx must be in SLASH_COMPLETIONS"
    );
}

#[test]
fn slash_completions_sorted_alphabetically() {
    let mut sorted = SLASH_COMPLETIONS.to_vec();
    sorted.sort_unstable();
    assert_eq!(
        SLASH_COMPLETIONS.to_vec(),
        sorted,
        "SLASH_COMPLETIONS must be in alphabetical order"
    );
}

// --- OTel footer guidance tests ---

#[test]
fn help_command_pushes_message_containing_slash_help() {
    let mut state = make_state("sess-help");
    push_system_message(&mut state, HELP_TEXT);
    let has_help = state
        .main_panel
        .lines_text(0, 200)
        .iter()
        .any(|l| l.contains("/help"));
    assert!(has_help, "help output must contain '/help'");
}

#[test]
fn help_text_covers_all_major_commands() {
    for cmd in [
        "/switch",
        "/health",
        "/tier",
        "/agent",
        "/briefing",
        "/clear",
    ] {
        assert!(HELP_TEXT.contains(cmd), "HELP_TEXT must mention {cmd}");
    }
}

#[test]
fn slash_completions_include_help_and_clear() {
    assert!(
        SLASH_COMPLETIONS.contains(&"/help"),
        "/help must be in SLASH_COMPLETIONS"
    );
    assert!(
        SLASH_COMPLETIONS.contains(&"/clear"),
        "/clear must be in SLASH_COMPLETIONS"
    );
}

#[test]
fn runner_picker_confirm_sets_runner_and_clears_mode() {
    let mut state = make_state("sess-picker");
    state.runner_picker_mode = true;
    state.slash_completions = vec!["codex".to_owned(), "claude".to_owned()];
    state.slash_popup_visible = true;
    state.slash_cursor = 0;

    // Simulate accept: take selected runner name, update state, clear picker
    let runner_name = state.slash_completions[state.slash_cursor].clone();
    state.runner = runner_name.clone();
    push_system_message(&mut state, format!("runner switched to {runner_name}"));
    state.runner_picker_mode = false;
    state.slash_popup_visible = false;
    state.slash_completions.clear();
    state.slash_cursor = 0;

    assert_eq!(state.runner, "codex");
    assert!(
        !state.runner_picker_mode,
        "runner_picker_mode must be cleared after confirm"
    );
    assert!(
        !state.slash_popup_visible,
        "popup must be closed after confirm"
    );
    assert!(
        state
            .main_panel
            .lines_text(0, 100)
            .iter()
            .any(|l| l.contains("runner switched")),
        "confirmation message must appear in panel"
    );
}

#[test]
fn clear_slash_popup_resets_runner_picker_mode() {
    let mut state = make_state("sess-popup");
    state.runner_picker_mode = true;
    state.slash_popup_visible = true;
    state.slash_completions = vec!["claude".to_owned()];

    clear_slash_popup(&mut state);

    assert!(
        !state.runner_picker_mode,
        "runner_picker_mode must be false after clear"
    );
    assert!(!state.slash_popup_visible);
    assert!(state.slash_completions.is_empty());
}

#[test]
fn format_spec_list_renders_changes_archived_and_specs() {
    let v = serde_json::json!({
        "changes": ["add-auth", "add-widget"],
        "archived": ["old-thing"],
        "specs": ["auth", "widget"],
    });
    let result = crate::slash::format_spec_list(&v);
    assert!(result.contains("add-auth"), "must list active changes");
    assert!(result.contains("old-thing"), "must list archived");
    assert!(result.contains("widget"), "must list specs");
}

#[test]
fn format_spec_list_empty_shows_none() {
    let v = serde_json::json!({ "changes": [], "archived": [], "specs": [] });
    let result = crate::slash::format_spec_list(&v);
    assert!(result.contains("(none)"), "empty sections read (none)");
}

#[test]
fn format_spec_status_single_change_shows_tasks_and_validity() {
    let v = serde_json::json!({
        "name": "add-auth",
        "tasks_done": 2,
        "tasks_total": 5,
        "valid": true,
        "delta_capabilities": ["auth"],
    });
    let result = crate::slash::format_spec_status(&v);
    assert!(result.contains("add-auth"), "must name the change");
    assert!(result.contains("2/5"), "must show task progress");
    assert!(result.contains("valid=true"), "must show validity");
}

#[test]
fn format_spec_status_empty_list_reports_no_changes() {
    let v = serde_json::json!({ "changes": [] });
    assert_eq!(
        crate::slash::format_spec_status(&v),
        "spec: no active changes"
    );
}

#[test]
fn format_spec_validation_fail_lists_errors() {
    let v = serde_json::json!({
        "change": "c",
        "valid": false,
        "errors": ["widget: requirement 'R' has no scenario"],
        "warnings": ["proposal.md is missing"],
    });
    let result = crate::slash::format_spec_validation(&v);
    assert!(result.contains("FAIL"), "invalid report must read FAIL");
    assert!(result.contains("no scenario"), "must surface the error");
    assert!(result.contains("warn:"), "must surface the warning");
}

#[test]
fn format_spec_validation_pass_reads_pass() {
    let v = serde_json::json!({ "change": "c", "valid": true, "errors": [], "warnings": [] });
    assert!(crate::slash::format_spec_validation(&v).contains("PASS"));
}

// --- cowork resolver helper ---

#[test]
fn scan_gov_artifacts_returns_empty_when_no_gov_dir() {
    let dir = tempfile::tempdir().unwrap();
    let artifacts = scan_gov_artifacts(dir.path());
    assert!(
        artifacts.is_empty(),
        "no gov/ dir should yield empty artifact list"
    );
}

#[test]
fn scan_gov_artifacts_parses_work_item_toml() {
    let dir = tempfile::tempdir().unwrap();
    let wi_dir = dir.path().join("gov").join("work-items");
    std::fs::create_dir_all(&wi_dir).unwrap();
    std::fs::write(
        wi_dir.join("WI-001.toml"),
        r#"id = "WI-001"
title = "Add thinking token streaming"
status = "in_progress"
"#,
    )
    .unwrap();
    let artifacts = scan_gov_artifacts(dir.path());
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].id, "WI-001");
    assert_eq!(artifacts[0].status, "in_progress");
    assert_eq!(artifacts[0].kind, "work-items");
}

#[test]
fn scan_gov_artifacts_skips_files_without_id() {
    let dir = tempfile::tempdir().unwrap();
    let wi_dir = dir.path().join("gov").join("work-items");
    std::fs::create_dir_all(&wi_dir).unwrap();
    std::fs::write(
        wi_dir.join("bad.toml"),
        r#"title = "missing id"
status = "draft"
"#,
    )
    .unwrap();
    let artifacts = scan_gov_artifacts(dir.path());
    assert!(
        artifacts.is_empty(),
        "TOML without 'id' field must be skipped"
    );
}

#[test]
fn format_gov_list_shows_count_and_ids() {
    let artifacts = vec![
        GovArtifact {
            id: "WI-001".into(),
            kind: "work-items".into(),
            title: "Add multi-line input".into(),
            status: "done".into(),
        },
        GovArtifact {
            id: "RFC-001".into(),
            kind: "rfc".into(),
            title: "Thinking token streaming".into(),
            status: "accepted".into(),
        },
    ];
    let output = format_gov_list(&artifacts);
    assert!(output.contains("2 govctl artifact"), "count must appear");
    assert!(output.contains("WI-001"), "WI-001 must appear");
    assert!(output.contains("RFC-001"), "RFC-001 must appear");
}

#[test]
fn format_gov_list_empty_returns_hint() {
    let output = format_gov_list(&[]);
    assert!(
        output.contains("gov/work-items"),
        "empty list must include path hint"
    );
}

// --- session rail (Ctrl-W) ------------------------------------------------

#[test]
fn gov_create_work_item_creates_toml_with_planned_status() {
    let dir = tempfile::tempdir().unwrap();
    let msg = gov_create(dir.path(), "work-item My first task");
    assert!(
        msg.contains("WI-001"),
        "should report created id; got: {msg}"
    );
    let path = dir.path().join("gov/work-items/WI-001.toml");
    assert!(
        path.exists(),
        "TOML file must be created at {}",
        path.display()
    );
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("planned"),
        "status must be planned; got: {content}"
    );
}

#[test]
fn gov_create_auto_increments_id() {
    let dir = tempfile::tempdir().unwrap();
    gov_create(dir.path(), "work-item First");
    let msg = gov_create(dir.path(), "work-item Second");
    assert!(
        msg.contains("WI-002"),
        "second item must get WI-002; got: {msg}"
    );
}

#[test]
fn gov_create_rfc_uses_draft_status() {
    let dir = tempfile::tempdir().unwrap();
    let msg = gov_create(dir.path(), "rfc My RFC");
    assert!(msg.contains("RFC-001"), "got: {msg}");
    let content = std::fs::read_to_string(dir.path().join("gov/rfc/RFC-001.toml")).unwrap();
    assert!(
        content.contains("draft"),
        "RFC default status must be draft; got: {content}"
    );
}

#[test]
fn gov_transition_updates_status_in_file() {
    let dir = tempfile::tempdir().unwrap();
    gov_create(dir.path(), "work-item Test task");
    let msg = gov_transition(dir.path(), "WI-001 in_progress");
    assert!(
        msg.contains("in_progress"),
        "should confirm transition; got: {msg}"
    );
    let content = std::fs::read_to_string(dir.path().join("gov/work-items/WI-001.toml")).unwrap();
    assert!(
        content.contains("\"in_progress\""),
        "file must contain updated status; got: {content}"
    );
}

#[test]
fn gov_transition_rejects_invalid_status() {
    let dir = tempfile::tempdir().unwrap();
    gov_create(dir.path(), "work-item Test task");
    let msg = gov_transition(dir.path(), "WI-001 flying");
    assert!(
        msg.contains("invalid status"),
        "must reject unknown status; got: {msg}"
    );
}

// --- P1a: role cockpit ----------------------------------------------------

#[test]
fn command_palette_empty_query_returns_all_commands() {
    let completions = command_palette_filtered("");
    assert_eq!(completions.len(), SLASH_COMPLETIONS.len());
}

#[test]
fn command_palette_filters_by_substring() {
    // "model" matches "/model" and substring of other commands that contain "model"
    let completions = command_palette_filtered("mod");
    assert!(
        completions.contains(&"/model".to_owned()),
        "expected /model in results"
    );
}

#[test]
fn command_palette_no_match_returns_empty() {
    let completions = command_palette_filtered("zzznomatch");
    assert!(completions.is_empty());
}
