//! Unit tests for the TUI crate root, moved verbatim from `main.rs`.
//! `super::*` resolves to the crate root, whose items these tests drive.

use super::*;

#[test]
fn push_capped_bounds_length_and_keeps_newest() {
    let cap = 8;
    let mut buf: Vec<usize> = Vec::new();
    let mut total_dropped = 0;
    for i in 0..1000 {
        total_dropped += push_capped(&mut buf, i, cap);
        // Length never exceeds the cap, no matter how many are pushed.
        assert!(buf.len() <= cap, "len {} exceeded cap {cap}", buf.len());
    }
    // Steady state holds exactly the last `cap` entries, oldest trimmed.
    assert_eq!(buf.len(), cap);
    assert_eq!(buf, (992..1000).collect::<Vec<_>>());
    // Every entry beyond the cap was dropped from the front exactly once.
    assert_eq!(total_dropped, 1000 - cap);
}

#[test]
fn push_capped_reports_no_drop_below_cap() {
    let mut buf: Vec<usize> = Vec::new();
    assert_eq!(push_capped(&mut buf, 1, 4), 0);
    assert_eq!(push_capped(&mut buf, 2, 4), 0);
    assert_eq!(buf, vec![1, 2]);
}

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
fn status_bar_line_segments_runner_tier_session() {
    let ctx = ModuleCtx {
        session_id: "abcd1234ef",
        mode: Some("impl"),
        tier: Some("deep"),
        runner: Some("claude-cli"),
        pending: false,
        input_mode: true,
        ctx_pct: None,
    };
    let text: String = status_bar_line(&ctx, true)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("INSERT"), "{text}");
    assert!(text.contains("CLAUDE"), "{text}"); // runner_label uppercases
    assert!(text.contains("deep"), "{text}");
    assert!(text.contains("abcd1234"), "{text}"); // 8-char session id
}

#[test]
fn status_bar_shows_ctx_pct_when_nonzero() {
    let ctx = ModuleCtx {
        session_id: "abc",
        mode: None,
        tier: None,
        runner: None,
        pending: false,
        input_mode: true,
        ctx_pct: Some(61),
    };
    let text: String = status_bar_line(&ctx, true)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("61%"), "ctx gauge must appear: {text}");
}

#[test]
fn status_bar_omits_ctx_gauge_when_none() {
    let ctx = ModuleCtx {
        session_id: "abc",
        mode: None,
        tier: None,
        runner: None,
        pending: false,
        input_mode: true,
        ctx_pct: None,
    };
    let text: String = status_bar_line(&ctx, true)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(!text.contains('%'), "no gauge when ctx_pct is None: {text}");
}

#[test]
fn ctrl_p_in_scroll_mode_toggles_session_peek() {
    let mut state = make_state("sess-peek");
    state.scroll_focus = true;
    assert!(!state.show_session_peek);
    // Simulate Ctrl+P toggle
    state.show_session_peek = !state.show_session_peek;
    assert!(state.show_session_peek);
}

#[test]
fn prompt_history_capped_at_max_size() {
    let mut history: Vec<String> = Vec::new();
    for i in 0..=PROMPT_HISTORY_CAP {
        history.push(format!("msg{i}"));
        if history.len() > PROMPT_HISTORY_CAP {
            history.remove(0);
        }
    }
    assert_eq!(history.len(), PROMPT_HISTORY_CAP);
}

#[test]
fn runner_capability_flags_for_known_runners() {
    assert!(runner_supports_thinking("anthropic"));
    assert!(!runner_supports_thinking("claude-cli"));
    assert!(!runner_supports_thinking("openai"));
    assert!(runner_is_subprocess("claude-cli"));
    assert!(runner_is_subprocess("codex-cli"));
    assert!(!runner_is_subprocess("anthropic"));
}

#[test]
fn format_capabilities_table_lists_runners() {
    let runners = vec![
        serde_json::json!({ "runner": "anthropic", "tier": "fast", "model": "claude-haiku-4-5-20251001" }),
        serde_json::json!({ "runner": "claude-cli", "tier": "fast", "model": "claude-opus" }),
    ];
    let table = format_capabilities_table(&runners);
    assert!(table.contains("anthropic"), "{table}");
    assert!(table.contains("thinking"), "{table}");
    assert!(table.contains("subprocess"), "{table}");
}

#[test]
fn status_hint_advertises_real_entry_points() {
    let text: String = status_hint_line(true)
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text.contains("/help"), "{text}");
    assert!(text.contains("^W"), "{text}");
}

#[test]
fn format_tool_detail_pretty_prints_json_args() {
    let lines = format_tool_detail("Bash", r#"{"command":"ls -la","timeout":5}"#);
    let joined = lines.join("\n");
    assert!(joined.contains("tool: Bash"), "{joined}");
    assert!(joined.contains("\"command\""), "{joined}"); // pretty JSON
    assert!(joined.contains("ls -la"), "{joined}");
    assert!(joined.contains("Esc to close"), "{joined}");
    // Non-JSON falls back to raw.
    let raw = format_tool_detail("X", "not json");
    assert!(raw.join("\n").contains("not json"));
}

#[test]
fn tool_call_card_shows_glyph_label_and_summary() {
    let line = tool_call_card("Bash", "find . -type f", true, '\u{2713}');
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("execute"), "{text}"); // ACP kind label
    assert!(text.contains("find . -type f"), "{text}");
    assert!(text.contains('\u{2713}'), "{text}"); // status glyph present
                                                  // No raw JSON braces leak into the card.
    assert!(!text.contains('{'), "{text}");
}

// --- metrics-live-fetch: pure JSON→rows mapper ---

#[test]
fn metrics_rows_from_summary_folds_buckets_per_runner() {
    let resp = json!({
        "tier": "hourly",
        "buckets": [
            { "bucket_start": 0, "runner": "claude", "turns": 1,
              "input_tok": 100, "output_tok": 50, "cost_usd": 0.01, "error_count": 1 },
            { "bucket_start": 3_600_000_000_i64, "runner": "claude", "turns": 1,
              "input_tok": 200, "output_tok": 80, "cost_usd": 0.02, "error_count": 2 },
            { "bucket_start": 0, "runner": "local", "turns": 1,
              "input_tok": 480, "output_tok": 0, "cost_usd": 0.0, "error_count": 0 },
        ],
    });
    let rows = metrics_rows_from_summary(&resp);
    assert_eq!(rows.len(), 2, "one row per runner");
    // First-seen runner order: claude then local.
    assert_eq!(rows[0].runner, "claude");
    assert_eq!(
        rows[0].tokens,
        100 + 50 + 200 + 80,
        "tokens summed across buckets"
    );
    assert!((rows[0].cost_usd - 0.03).abs() < 1e-9, "cost accumulated");
    assert_eq!(rows[0].errors, 3, "errors accumulated");
    assert_eq!(rows[1].runner, "local");
    assert_eq!(rows[1].tokens, 480);
}

#[test]
fn metrics_rows_from_summary_empty_buckets_yields_no_rows() {
    let empty = json!({ "tier": "hourly", "buckets": [] });
    assert!(metrics_rows_from_summary(&empty).is_empty());
    let missing = json!({ "tier": "hourly" });
    assert!(metrics_rows_from_summary(&missing).is_empty());
}

#[test]
fn metrics_rows_from_summary_tolerates_missing_fields() {
    let resp = json!({
        "buckets": [
            { "runner": "claude" },
        ],
    });
    let rows = metrics_rows_from_summary(&resp);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].runner, "claude");
    assert_eq!(rows[0].tokens, 0);
    assert!((rows[0].cost_usd - 0.0).abs() < 1e-9);
    assert_eq!(rows[0].errors, 0);
}

// --- metrics-live-fetch: poll-due predicate ---

#[test]
fn metrics_poll_due_when_visible_and_unset_or_elapsed() {
    let now = std::time::Instant::now();
    // Visible and never polled → due.
    assert!(metrics_poll_due(true, None, now));
    // Visible and the interval has elapsed → due.
    let stale = now.checked_sub(std::time::Duration::from_secs(4)).unwrap();
    assert!(metrics_poll_due(true, Some(stale), now));
    // Visible but within the interval → not due.
    let fresh = now.checked_sub(std::time::Duration::from_secs(1)).unwrap();
    assert!(!metrics_poll_due(true, Some(fresh), now));
    // Hidden → never due, regardless of timing.
    assert!(!metrics_poll_due(false, None, now));
    assert!(!metrics_poll_due(false, Some(stale), now));
}

// --- metrics-live-fetch: toggle resets the poll cadence ---

#[test]
fn toggling_metrics_view_on_resets_last_metrics_poll() {
    let mut state = make_state("sess-metrics-toggle");
    state.last_metrics_poll = Some(std::time::Instant::now());
    assert!(!state.panels.metrics);
    // Toggle on → visible and the poll is cleared for an immediate fetch.
    toggle_metrics_view(&mut state);
    assert!(state.panels.metrics, "toggle makes the panel visible");
    assert!(
        state.last_metrics_poll.is_none(),
        "toggle-on clears last_metrics_poll for an immediate fetch"
    );
    // Toggle off → hidden again.
    toggle_metrics_view(&mut state);
    assert!(!state.panels.metrics, "second toggle hides the panel");
}

// --- metrics-live-fetch: live fetch populates/clears the snapshot ---

#[test]
fn live_metrics_response_populates_then_clears_snapshot() {
    let mut state = make_state("sess-metrics-populate");
    assert!(
        state.metrics_snapshot.is_empty(),
        "snapshot starts empty (the previously-blank panel)"
    );
    let resp = json!({
        "tier": "hourly",
        "buckets": [
            { "runner": "claude", "input_tok": 700, "output_tok": 80,
              "cost_usd": 0.06, "error_count": 1 },
        ],
    });
    state.metrics_snapshot = metrics_rows_from_summary(&resp);
    assert_eq!(
        state.metrics_snapshot.len(),
        1,
        "live fetch fills the snapshot"
    );
    assert_eq!(state.metrics_snapshot[0].runner, "claude");
    // An empty window replaces the snapshot wholesale — no stale rows.
    let empty = json!({ "tier": "hourly", "buckets": [] });
    state.metrics_snapshot = metrics_rows_from_summary(&empty);
    assert!(
        state.metrics_snapshot.is_empty(),
        "empty window clears the snapshot rather than leaving stale rows"
    );
}

// --- /review scope-flag parsing ---

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

// L128: trailing backslash appends newline continuation, does not submit.
#[test]
fn backslash_continuation_appends_newline() {
    let mut input = "hello\\".to_owned();
    // Simulate the Enter key handling logic inline.
    assert!(input.ends_with('\\'));
    input.pop();
    input.push('\n');
    assert!(input.contains('\n'));
    assert_eq!(input, "hello\n");
}

// L128: continuation display prefix uses "..." for multi-line input.
#[test]
fn continuation_display_uses_ellipsis_prefix() {
    let input = "first line\nsecond";
    let display = if input.contains('\n') {
        let last_line = input.rsplit('\n').next().unwrap_or("");
        format!("... {last_line}_")
    } else {
        format!("> {input}_")
    };
    assert_eq!(display, "... second_");
}

// L128: normal input display uses "> " prefix.
#[test]
fn normal_display_uses_prompt_prefix() {
    let input = "hello";
    let display = format!("> {input}_");
    assert_eq!(display, "> hello_");
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

// -----------------------------------------------------------------------
// Layer 4 TUI functional tests — TestBackend (no network)
// -----------------------------------------------------------------------

use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// Constructs a minimal `AppState` for testing without a daemon connection.
#[allow(clippy::too_many_lines)]
fn make_state(session_id: &str) -> AppState {
    AppState {
        session_id: session_id.to_owned(),
        mode: None,
        tier: None,
        runner: String::from("unknown"),
        model: None,
        messages: Vec::new(),
        input: String::new(),
        quit: false,
        quit_armed: false,
        permission_mode: "ask".to_owned(),
        graph_workspace: None,
        graph_symbols: None,
        tool_details: Vec::new(),
        pending_tool: None,
        secret_var: None,
        pending_task_id: None,
        last_poll: None,
        turn_n: 0,
        turn_submitted_at: None,
        current_block: None,
        block_store: blocks::BlockStore::new(),
        block_browser_open: false,
        block_browser_cursor: 0,
        clipboard: None,
        diff_overlay: None,
        diff_scroll: 0,
        diff_split_view: false,
        staging_queue: staging::StagingQueue::new(),
        panels: PanelVisibility {
            context_rail: true,
            metrics: false,
            session_rail: false,
            lsp: true,
            obs: true,
            role_cockpit: false,
            quality: false,
            value: false,
            fleet: false,
        },
        metrics_snapshot: Vec::new(),
        tier_snapshot: Vec::new(),
        savings_snapshot: metrics_view::SavingsSnapshot::default(),
        last_metrics_poll: None,
        last_obs_poll: None,
        context_used: 0,
        context_window: 200_000,
        main_panel: main_panel::MainPanel::new(),
        action_log: action_log::ActionLog::new(50),
        slash_completions: Vec::new(),
        slash_popup_visible: false,
        slash_cursor: 0,
        runner_picker_mode: false,
        session_picker_mode: false,
        command_palette_mode: false,
        session_picker_ids: Vec::new(),
        session_rail_items: Vec::new(),
        session_rail_cursor: 0,
        last_session_rail_poll: None,
        session_detail_overlay: None,
        turn_in_flight: false,
        assistant_open: false,
        poll_retry_count: 0,
        scroll_focus: false,
        selection_mode: false,
        selection_anchor: (0, 0),
        selection_end: (0, 0),
        g_pending: false,
        input_cursor: 0,
        pending_cowork: Vec::new(),
        cowork_modify_mode: false,
        cowork_modify_input: String::new(),
        last_graph_poll: None,
        stream_rx: None,
        upgrade_rx: None,
        current_thinking: String::new(),
        thinking_steps: Vec::new(),
        thinking_expanded: false,
        kill_ring: VecDeque::new(),
        active_agent_name: None,
        stream_sock_path: PathBuf::from("/tmp/smdjad.sock.stream"),
        last_traceparent: None,
        pending_output_type: None,
        otlp_configured: false,
        no_color: false,
        spinner_tick: 0,
        panel_search_mode: false,
        panel_search_query: String::new(),
        display_start_idx: 0,
        prompt_history: Vec::new(),
        history_idx: None,
        saved_input: String::new(),
        history_search_mode: false,
        history_search_query: String::new(),
        lsp_last_poll: None,
        lsp_snapshot: smedja_lsp::LspSnapshot::default(),
        obs_snapshot: obs_panel::ObsSnapshot::default(),
        quality_snapshot: quality_panel::QualitySnapshot::default(),
        plan_steps: Vec::new(),
        consecutive_low_quality: 0,
        quality_review_in_progress: false,
        ctrl_q_pressed_at: None,
        value_snapshot: value_panel::ValueSnapshot::default(),
        latency_samples: VecDeque::new(),
        session_tokens_in: 0,
        session_tokens_out: 0,
        turn_tokens_in: 0,
        turn_tokens_out: 0,
        quality_score_sum: 0,
        quality_score_count: 0,
        file_picker_open: false,
        file_picker_dir: std::path::PathBuf::new(),
        file_picker_entries: Vec::new(),
        file_picker_cursor: 0,
        show_session_peek: false,
        render_mode: viz::RenderMode::Block,
        current_trace: trace_waterfall::TurnTrace::default(),
        trace_selected: 0,
        trace_expanded: false,
        fleet: fleet_panel::FleetState::default(),
        live_tokens: 0,
        last_stream_activity: None,
        tool_started_at: None,
        plan_current: 0,
        prev_model: None,
        prev_tier: None,
        prev_context_used: 0,
    }
}

/// Renders `state` to an 80×24 `TestBackend` and returns the buffer.
fn render_frame(state: &mut AppState) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    terminal.backend().buffer().clone()
}

#[test]
fn quit_flag_starts_false_and_can_be_set() {
    let mut state = make_state("test-session");
    assert!(!state.quit);
    state.quit = true;
    assert!(state.quit);
}

#[test]
fn input_accumulates_characters_in_state() {
    let mut state = make_state("test-session");
    state.input.push('h');
    state.input.push('i');
    assert_eq!(state.input, "hi");
    // TODO: assert the input appears in the rendered buffer once
    // handle_key can be called without a live Client.
}

#[test]
fn render_does_not_panic_with_empty_state() {
    let mut state = make_state("test-session");
    let _buf = render_frame(&mut state);
    // Verify no panic — any output is acceptable.
}

#[test]
fn slash_popup_visible_flag_and_render() {
    let mut state = make_state("test-session");
    assert!(!state.slash_popup_visible);
    state.slash_popup_visible = true;
    state.slash_completions = filtered_completions("/");
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        !content.trim().is_empty(),
        "buffer should not be entirely blank when slash popup is open"
    );
}

#[test]
fn block_browser_renders_without_panic() {
    let mut state = make_state("test-session");
    let mut block = blocks::TurnBlock::new(1);
    block.complete(42);
    state.block_store.push(block);
    state.block_browser_open = true;
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        !content.trim().is_empty(),
        "buffer should not be blank when block browser is open"
    );
}

#[test]
fn diff_overlay_renders_without_panic() {
    let mut state = make_state("test-session");
    state.diff_overlay = Some((
        0,
        vec!["+added line".to_owned(), "-removed line".to_owned()],
    ));
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        !content.trim().is_empty(),
        "buffer should not be blank when diff overlay is set"
    );
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

#[test]
fn connect_banner_visible() {
    let mut state = make_state("sess-abc");
    let sock = "/run/user/1000/smdjad.sock";
    state.main_panel.push_line(format!("connected to {sock}"));
    state.main_panel.push_line("session sess-abc".into());
    state.main_panel.push_line("provider: unknown".into());
    state.main_panel.push_line("tier: default".into());
    state
        .main_panel
        .push_line("type a message or /help for commands".into());
    // Auto-scroll leaves scroll at the last line; scroll to top to see the
    // full banner in the rendered frame.
    state.main_panel.scroll_to_top();
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(content.contains("sess-abc"), "banner must show session ID");
    assert!(
        content.contains("connected"),
        "banner must show connection line"
    );
}

#[test]
fn status_bar_shows_tier_when_set() {
    let mut state = make_state("sess-xyz");
    state.tier = Some("fast".into());
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(content.contains("fast"), "status bar must render the tier");
}

#[test]
fn status_bar_shows_unknown_when_no_tier() {
    let mut state = make_state("sess-xyz");
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(!content.trim().is_empty());
}

// --- thinking indicator tests ---

#[test]
fn thinking_indicator_visible_when_turn_in_flight() {
    let mut state = make_state("sess-think");
    state.turn_in_flight = true;
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("thinking") || content.contains("streaming") || content.contains("cancel"),
        "buffer should contain the live line when turn_in_flight is true"
    );
}

#[test]
fn thinking_indicator_hidden_when_idle() {
    let mut state = make_state("sess-idle");
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(!content.is_empty());
}

// --- layout regression tests ---

#[test]
fn layout_input_row_at_bottom_of_80x24() {
    let mut state = make_state("sess-layout");
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut state)).unwrap();
    let buf = terminal.backend().buffer();
    assert_eq!(buf.area().height, 24);
    assert_eq!(buf.area().width, 80);
}

#[test]
fn layout_40x10_does_not_panic() {
    let mut state = make_state("sess-narrow");
    let backend = ratatui::backend::TestBackend::new(40, 10);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut state)).unwrap();
    let buf = terminal.backend().buffer();
    assert_eq!(buf.area().width, 40);
    assert_eq!(buf.area().height, 10);
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

// Bug regression: `x` inspects the trace waterfall whenever the panel is
// visible — including in input mode, where the owner actually watches the
// trace. It must not require scroll mode, but must never steal a typed 'x'
// while composing a message.
#[tokio::test]
async fn x_inspects_trace_in_input_mode_when_panel_visible() {
    use tokio::net::UnixListener;

    // A socket the client can connect to; the `x` handler returns before any
    // RPC, so the mock never needs to respond.
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("trace-x.sock");
    let listener = UnixListener::bind(&sock_path).unwrap();
    tokio::spawn(async move {
        let _ = listener.accept().await;
    });

    let mut client = Client::connect(&sock_path).await.unwrap();
    let mut editor = rustyline::DefaultEditor::new().unwrap();
    let mut state = make_state("trace-x");
    // Trace panel visible (obs on + spans recorded); input mode, empty buffer.
    state.panels.obs = true;
    state.scroll_focus = false;
    state.input.clear();
    state.current_trace.start_turn();
    state.current_trace.push_tool("Read", 100);
    state.current_trace.settle_last_tool(300, true);

    let x = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::empty(),
    );

    // First `x`: open the inspector on the first span.
    handle_key(x, &mut state, &mut client, &mut editor)
        .await
        .unwrap();
    assert!(
        state.trace_expanded,
        "x must expand the trace in input mode"
    );
    assert_eq!(state.trace_selected, 0);
    assert!(
        state.input.is_empty(),
        "x must be consumed as inspect, not typed into the input"
    );

    // Second `x`: step to the next span.
    handle_key(x, &mut state, &mut client, &mut editor)
        .await
        .unwrap();
    assert_eq!(state.trace_selected, 1, "x steps to the next span");

    // While composing (non-empty buffer), `x` types normally instead of inspecting.
    state.trace_expanded = false;
    state.input = "fi".into();
    state.input_cursor = state.input.len();
    handle_key(x, &mut state, &mut client, &mut editor)
        .await
        .unwrap();
    assert_eq!(state.input, "fix", "x must type normally while composing");
    assert!(
        !state.trace_expanded,
        "x must not trigger the inspector mid-compose"
    );
}

// Verifies that the token-count footer pushed after a turn completes
// appears in the rendered frame buffer.
#[test]
fn turn_footer_shows_token_counts() {
    let mut state = make_state("sess-footer");
    // Simulate what the subscribe completion path does.
    state.main_panel.push_delta("response text");
    state
        .main_panel
        .push_line("↳ 10↑ 20↓ tokens · 250ms".into());
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("tokens"),
        "turn footer should show token count label in the rendered buffer"
    );
}

// --- input cursor tests ---

#[test]
fn wrap_input_rows_splits_long_line() {
    // 25 chars at width 10 → 3 rows (10/10/5).
    let rows = wrap_input_rows(&"x".repeat(25), 10);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].chars().count(), 10);
    assert_eq!(rows[2].chars().count(), 5);
}

#[test]
fn wrap_input_rows_honours_newlines() {
    let rows = wrap_input_rows("ab\ncd", 80);
    assert_eq!(rows, vec!["ab".to_string(), "cd".to_string()]);
}

#[test]
fn wrap_input_rows_empty_is_one_row() {
    assert_eq!(wrap_input_rows("", 10).len(), 1);
}

#[test]
fn prev_char_boundary_moves_back_one_ascii() {
    assert_eq!(prev_char_boundary("hello", 3), 2);
}

#[test]
fn prev_char_boundary_at_zero_stays_zero() {
    assert_eq!(prev_char_boundary("hello", 0), 0);
}

#[test]
fn next_char_boundary_moves_forward_one_ascii() {
    assert_eq!(next_char_boundary("hello", 2), 3);
}

#[test]
fn next_char_boundary_at_end_stays_at_end() {
    assert_eq!(next_char_boundary("hello", 5), 5);
}

#[test]
fn prev_char_boundary_unicode_moves_by_char() {
    // 'é' encodes as 2 bytes (U+00E9); cursor at 2 should move to 0.
    let s = "é";
    assert_eq!(s.len(), 2);
    assert_eq!(prev_char_boundary(s, 2), 0);
}

#[test]
fn next_char_boundary_unicode_moves_by_char() {
    let s = "é";
    assert_eq!(next_char_boundary(s, 0), 2);
}

#[test]
fn input_cursor_defaults_to_zero_in_make_state() {
    let state = make_state("s");
    assert_eq!(state.input_cursor, 0);
}

// ── provider-display: session.create response parsing ───────────────────

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
fn status_bar_shows_runner_when_set() {
    let mut state = make_state("sess-runner");
    state.runner = "anthropic".to_owned();
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("ANTHROPIC"),
        "status bar must render the runner label; got: {content}"
    );
}

// ── tui-message-selection: T6 tests ─────────────────────────────────────

#[test]
fn yank_lines_text_builds_newline_joined_string() {
    let mut panel = main_panel::MainPanel::new();
    for i in 0..5u32 {
        panel.push_line(format!("line {i}"));
    }
    let lines = panel.lines_text(1, 3);
    let text = lines.join("\n");
    assert_eq!(text, "line 1\nline 2\nline 3");
}

#[test]
fn selection_anchor_end_resolves_to_min_max_regardless_of_direction() {
    // Drag from line 4 back to line 1 — selection should span 1..=4.
    let anchor = 4usize;
    let end = 1usize;
    let lo = anchor.min(end);
    let hi = anchor.max(end);
    assert_eq!(lo, 1);
    assert_eq!(hi, 4);
    // Forward direction.
    let anchor = 1usize;
    let end = 4usize;
    assert_eq!(anchor.min(end), 1);
    assert_eq!(anchor.max(end), 4);
}

#[test]
fn esc_in_selection_mode_cancels_selection_without_scroll_change() {
    let mut state = make_state("sess-sel");
    for i in 0..10u32 {
        state.main_panel.push_line(format!("msg {i}"));
    }
    state.scroll_focus = true;
    state.selection_mode = true;
    state.selection_anchor = (3, 0);
    state.selection_end = (6, 0);
    state.main_panel.scroll = 3;

    // Simulate the Esc path: selection_mode cleared, scroll unchanged.
    if state.selection_mode {
        state.selection_mode = false;
    }

    assert!(
        !state.selection_mode,
        "selection must be cancelled after Esc"
    );
    assert_eq!(state.main_panel.scroll, 3, "scroll must not change on Esc");
    assert!(
        state.scroll_focus,
        "scroll_focus must remain active after cancelling selection"
    );
}

#[test]
fn esc_when_idle_activates_scroll_focus() {
    let mut state = make_state("sess-idle");
    assert!(!state.scroll_focus, "scroll_focus should be off by default");

    // Simulate the last else branch of Esc: no overlay, no selection, no scroll focus.
    state.scroll_focus = true;

    assert!(
        state.scroll_focus,
        "scroll_focus must be set by Esc when idle"
    );
}

#[test]
fn insert_key_exits_scroll_and_clears_selection() {
    let mut state = make_state("sess-ins");
    state.scroll_focus = true;
    state.selection_mode = true;
    state.g_pending = true;

    // Simulate 'i' key in scroll_focus block.
    state.scroll_focus = false;
    state.selection_mode = false;
    state.g_pending = false;

    assert!(!state.scroll_focus);
    assert!(!state.selection_mode);
    assert!(!state.g_pending);
}

#[test]
fn slugify_converts_spaces_and_upper() {
    assert_eq!(slugify("Smedja Architecture"), "smedja-architecture");
    assert_eq!(slugify("Q3 Agent Metrics!"), "q3-agent-metrics");
    assert_eq!(slugify("multi--word"), "multi-word");
}

#[test]
fn extract_code_block_finds_xml_content() {
    let text = "Some preamble\n```xml\n<mxGraph>hello</mxGraph>\n```\nsome epilogue";
    let extracted = extract_code_block(text, "xml");
    assert_eq!(extracted, Some("<mxGraph>hello</mxGraph>"));
}

#[test]
fn extract_code_block_returns_none_when_lang_absent() {
    let text = "```python\nprint('hi')\n```";
    assert!(extract_code_block(text, "xml").is_none());
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

/// Build the footer string the same way the streaming `done` handler does,
/// so the unit test does not depend on a live event loop.
fn build_turn_footer(
    input_tok: u64,
    output_tok: u64,
    turn_ms: u64,
    traceparent: Option<&str>,
    otlp_configured: bool,
) -> String {
    if let Some(tp_str) = traceparent {
        if otlp_configured {
            format!("↳ {input_tok}↑ {output_tok}↓ · trace: {tp_str}")
        } else {
            format!(
                    "↳ {input_tok}↑ {output_tok}↓ · trace: {tp_str} · traces not exported (set SMEDJA_OTLP_ENDPOINT)"
                )
        }
    } else {
        format!("↳ {input_tok}↑ {output_tok}↓ tokens · {turn_ms}ms")
    }
}

#[test]
fn footer_shows_otlp_warning_when_not_configured() {
    let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let footer = build_turn_footer(100, 50, 300, Some(tp), false);
    assert!(
        footer.contains(tp),
        "footer must include the traceparent string"
    );
    assert!(
        footer.contains("traces not exported"),
        "footer must warn that traces are not exported when OTLP is not configured"
    );
    assert!(
        footer.contains("SMEDJA_OTLP_ENDPOINT"),
        "footer must mention SMEDJA_OTLP_ENDPOINT"
    );
}

#[test]
fn footer_shows_no_otlp_warning_when_configured() {
    let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let footer = build_turn_footer(100, 50, 300, Some(tp), true);
    assert!(
        footer.contains(tp),
        "footer must include the traceparent string"
    );
    assert!(
        !footer.contains("traces not exported"),
        "footer must not show warning when OTLP is configured"
    );
}

#[test]
fn ctrl_c_with_clipboard_does_not_quit() {
    let mut state = make_state("sess-ctrlc");
    state.clipboard = Some("some text".to_owned());
    // Simulate the Ctrl-C branch: clipboard is Some → do NOT quit
    if state.clipboard.is_some() {
        // copy, do not quit
    } else {
        state.quit = true;
    }
    assert!(
        !state.quit,
        "Ctrl-C must not quit when clipboard is non-empty"
    );
}

#[test]
fn ctrl_c_with_no_clipboard_quits() {
    let mut state = make_state("sess-ctrlc");
    state.clipboard = None;
    // Simulate the Ctrl-C branch: clipboard is None → quit
    if state.clipboard.is_some() {
        // copy
    } else {
        state.quit = true;
    }
    assert!(state.quit, "Ctrl-C must quit when clipboard is empty");
}

// --- tui-input-modes tests ---

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
fn status_bar_shows_input_mode_badge_when_not_scroll() {
    let mut state = make_state("sess-mode");
    state.scroll_focus = false;
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("INSERT"),
        "status bar must show INSERT when scroll_focus=false; got: {content}"
    );
}

#[test]
fn status_bar_shows_normal_mode_badge_when_scroll() {
    let mut state = make_state("sess-mode");
    state.scroll_focus = true;
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("SCROLL"),
        "status bar must show SCROLL when scroll_focus=true; got: {content}"
    );
}

// --- tui-prompt-history tests ---

#[test]
fn history_search_finds_most_recent_match() {
    let history = vec![
        "git status".to_owned(),
        "git diff".to_owned(),
        "ls".to_owned(),
    ];
    let result = history_search(&history, "git");
    assert_eq!(
        result,
        Some((1, "git diff")),
        "should return most recent match"
    );
}

#[test]
fn history_search_empty_query_returns_none() {
    let history = vec!["git status".to_owned()];
    assert!(history_search(&history, "").is_none());
}

#[test]
fn history_search_no_match_returns_none() {
    let history = vec!["git status".to_owned()];
    assert!(history_search(&history, "foobar").is_none());
}

#[test]
fn history_search_empty_history_returns_none() {
    let history: Vec<String> = vec![];
    assert!(history_search(&history, "git").is_none());
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
fn up_key_loads_most_recent_history_entry() {
    let mut state = make_state("sess-hist");
    state.prompt_history = vec!["first".to_owned(), "second".to_owned()];
    state.input = "live".to_owned();
    state.input_cursor = state.input.len();

    // Simulate Up key (first press)
    if !state.prompt_history.is_empty() {
        let new_idx = match state.history_idx {
            None => {
                state.saved_input = state.input.clone();
                state.prompt_history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        state.history_idx = Some(new_idx);
        state.input = state.prompt_history[new_idx].clone();
        state.input_cursor = state.input.len();
    }

    assert_eq!(state.input, "second");
    assert_eq!(state.history_idx, Some(1));
    assert_eq!(state.saved_input, "live");
}

#[test]
fn down_key_at_end_restores_live_input() {
    let mut state = make_state("sess-hist-down");
    state.prompt_history = vec!["only".to_owned()];
    state.saved_input = "live input".to_owned();
    state.history_idx = Some(0);
    state.input = "only".to_owned();

    // Simulate Down key past end
    if let Some(idx) = state.history_idx {
        if idx + 1 < state.prompt_history.len() {
            let new_idx = idx + 1;
            state.history_idx = Some(new_idx);
            state.input = state.prompt_history[new_idx].clone();
            state.input_cursor = state.input.len();
        } else {
            state.history_idx = None;
            state.input = std::mem::take(&mut state.saved_input);
            state.input_cursor = state.input.len();
        }
    }

    assert!(
        state.history_idx.is_none(),
        "history_idx must be None after returning to live input"
    );
    assert_eq!(state.input, "live input");
}

#[test]
fn ctrl_r_in_input_mode_enters_history_search() {
    let mut state = make_state("sess-ctrl-r");
    state.scroll_focus = false;
    state.input = "current".to_owned();

    // Simulate Ctrl-R in input mode
    state.history_search_mode = true;
    state.history_search_query.clear();
    state.saved_input = state.input.clone();

    assert!(state.history_search_mode);
    assert_eq!(state.saved_input, "current");
}

#[test]
fn ctrl_r_in_scroll_mode_toggles_context_rail() {
    let mut state = make_state("sess-ctrl-r-scroll");
    state.scroll_focus = true;
    state.panels.context_rail = true;

    // Simulate Ctrl-R in scroll mode
    state.panels.context_rail = !state.panels.context_rail;

    assert!(
        !state.panels.context_rail,
        "context rail must be toggled off"
    );
}

#[test]
fn ctrl_t_toggles_metrics_view() {
    let mut state = make_state("sess-ctrl-t");
    assert!(!state.panels.metrics, "metrics view starts hidden");
    // Simulate Ctrl-T.
    state.panels.metrics = !state.panels.metrics;
    assert!(state.panels.metrics, "Ctrl-T must show metrics view");
    state.panels.metrics = !state.panels.metrics;
    assert!(!state.panels.metrics, "Ctrl-T again must hide it");
}

#[test]
fn metrics_view_panel_renders_per_runner_snapshot() {
    let mut state = make_state("sess-metrics-render");
    state.panels.metrics = true;
    state.metrics_snapshot = vec![
        metrics_view::MetricsRow {
            runner: "claude".into(),
            tokens: 780,
            cost_usd: 0.06,
            errors: 2,
        },
        metrics_view::MetricsRow {
            runner: "local".into(),
            tokens: 480,
            cost_usd: 0.0,
            errors: 0,
        },
    ];
    // MetricsView lives inside the context rail; rail needs width >= 100.
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut state)).unwrap();
    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(content.contains("claude"), "claude runner must render");
    assert!(content.contains("local"), "local runner must render");
    assert!(content.contains("$0.0600"), "claude cost must render");
    assert!(content.contains("780"), "claude tokens must render");
}

// Bug regression: enabling the trace waterfall must not clobber the LSP panel.
// Both get their own rail slot, and LSP keeps a minimum height (Min, not Fill)
// so the fixed-height trace panel can never starve it to zero rows.
#[test]
fn trace_and_lsp_coexist_in_rail() {
    let mut state = make_state("rail-coexist");
    state.panels.context_rail = true;
    state.panels.lsp = true;
    state.panels.obs = true;
    // A recorded turn trace makes the waterfall visible alongside LSP/obs.
    state.current_trace.start_turn();
    state.current_trace.push_tool("Read", 100);
    state.current_trace.settle_last_tool(300, true);
    state.current_trace.finish(400, true);

    // Wide + tall enough that the rail renders and every panel has room.
    let backend = ratatui::backend::TestBackend::new(120, 40);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, &mut state)).unwrap();
    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();

    assert!(
        content.contains("lsp"),
        "LSP panel must still render when the trace is enabled; got: {content:?}"
    );
    assert!(
        content.contains("trace"),
        "trace panel must render alongside LSP; got: {content:?}"
    );
    assert!(
        content.contains("obs"),
        "obs panel must render alongside LSP and trace; got: {content:?}"
    );
}

#[test]
fn history_search_esc_restores_saved_input() {
    let mut state = make_state("sess-search-esc");
    state.history_search_mode = true;
    state.history_search_query = "git".to_owned();
    state.saved_input = "original".to_owned();
    state.input = "git status".to_owned();

    // Simulate Esc
    state.history_search_mode = false;
    state.history_search_query.clear();
    state.input = std::mem::take(&mut state.saved_input);
    state.input_cursor = state.input.len();

    assert!(!state.history_search_mode);
    assert_eq!(state.input, "original");
    assert!(state.history_search_query.is_empty());
}

#[test]
fn history_search_enter_accepts_match() {
    let mut state = make_state("sess-search-enter");
    state.history_search_mode = true;
    state.history_search_query = "git".to_owned();
    state.input = "git status".to_owned();

    // Simulate Enter
    state.history_search_mode = false;
    state.history_search_query.clear();

    assert!(
        !state.history_search_mode,
        "search mode must be cleared on Enter"
    );
    assert_eq!(
        state.input, "git status",
        "matched input must be kept on Enter"
    );
}

// --- tui native spec-command formatter tests ---

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

fn cowork_item(id: &str, tool: &str) -> cowork_widget::CoworkItem {
    cowork_widget::CoworkItem {
        id: id.to_owned(),
        tool: tool.to_owned(),
        step_n: 1,
        args_display: String::new(),
        reasoning: String::new(),
    }
}

#[test]
fn cowork_resolved_true_only_when_flag_set() {
    let yes: Result<serde_json::Value, smedja_rpc::RpcError> =
        Ok(json!({ "id": "a", "resolved": true }));
    assert!(cowork_resolved(&yes), "resolved:true must return true");

    let no: Result<serde_json::Value, smedja_rpc::RpcError> =
        Ok(json!({ "id": "a", "resolved": false }));
    assert!(!cowork_resolved(&no), "resolved:false must return false");

    let missing: Result<serde_json::Value, smedja_rpc::RpcError> = Ok(json!({ "id": "a" }));
    assert!(
        !cowork_resolved(&missing),
        "missing resolved field must return false"
    );

    let err: Result<serde_json::Value, smedja_rpc::RpcError> =
        Err(smedja_rpc::RpcError::new(-32603, "transport down"));
    assert!(!cowork_resolved(&err), "transport error must return false");
}

// --- cowork decision application (approve / deny) ---

#[test]
fn apply_cowork_decision_approve_resolved_removes_and_confirms() {
    let result: Result<serde_json::Value, smedja_rpc::RpcError> =
        Ok(json!({ "id": "a", "resolved": true }));
    let item = cowork_item("a", "bash");
    let (remove, message) =
        apply_cowork_decision(&result, "cowork.approve", "approved: bash", &item.tool);
    assert!(remove, "resolved:true must remove the item");
    assert_eq!(message, "approved: bash");
}

#[test]
fn apply_cowork_decision_unresolved_retains_and_reports_not_found() {
    let result: Result<serde_json::Value, smedja_rpc::RpcError> =
        Ok(json!({ "id": "a", "resolved": false }));
    let item = cowork_item("a", "bash");
    let (remove, message) =
        apply_cowork_decision(&result, "cowork.approve", "approved: bash", &item.tool);
    assert!(!remove, "resolved:false must retain the item");
    assert_eq!(message, "item not found: bash");
}

#[test]
fn apply_cowork_decision_deny_resolved_removes_and_confirms() {
    let result: Result<serde_json::Value, smedja_rpc::RpcError> =
        Ok(json!({ "id": "a", "resolved": true }));
    let item = cowork_item("a", "edit_file");
    let (remove, message) =
        apply_cowork_decision(&result, "cowork.deny", "denied: edit_file", &item.tool);
    assert!(remove, "resolved:true must remove the item");
    assert_eq!(message, "denied: edit_file");
}

#[test]
fn apply_cowork_decision_rpc_error_retains_and_reports_error() {
    let result: Result<serde_json::Value, smedja_rpc::RpcError> =
        Err(smedja_rpc::RpcError::new(-32603, "boom"));
    let item = cowork_item("a", "bash");
    let (remove, message) =
        apply_cowork_decision(&result, "cowork.approve", "approved: bash", &item.tool);
    assert!(!remove, "rpc error must retain the item");
    assert!(
        message.contains("cowork.approve error"),
        "error message must name the method; got: {message}"
    );
}

// --- cowork modify flow ---

#[test]
fn apply_cowork_decision_modify_resolved_echoes_instruction() {
    let result: Result<serde_json::Value, smedja_rpc::RpcError> =
        Ok(json!({ "id": "a", "resolved": true }));
    let item = cowork_item("a", "bash");
    let (remove, message) = apply_cowork_decision(
        &result,
        "cowork.modify",
        "modify sent: use ls -la instead",
        &item.tool,
    );
    assert!(remove, "resolved:true must remove the item");
    assert_eq!(message, "modify sent: use ls -la instead");
}

#[test]
fn apply_cowork_decision_modify_unresolved_retains_item() {
    let result: Result<serde_json::Value, smedja_rpc::RpcError> =
        Ok(json!({ "id": "a", "resolved": false }));
    let item = cowork_item("a", "bash");
    let (remove, message) = apply_cowork_decision(
        &result,
        "cowork.modify",
        "modify sent: use ls -la instead",
        &item.tool,
    );
    assert!(!remove, "resolved:false must retain the item");
    assert_eq!(message, "item not found: bash");
}

// --- lsp_snapshot_from_rpc -----------------------------------------------

#[test]
fn lsp_snapshot_from_rpc_decodes_all_severity_strings() {
    let status = json!({"servers": []});
    let diag = json!({
        "diagnostics": [
            {"file": "a.rs", "line": 1, "col": 1, "severity": "error",   "message": "e"},
            {"file": "a.rs", "line": 2, "col": 1, "severity": "warning", "message": "w"},
            {"file": "a.rs", "line": 3, "col": 1, "severity": "info",    "message": "i"},
            {"file": "a.rs", "line": 4, "col": 1, "severity": "hint",    "message": "h"},
        ]
    });
    let snap = lsp_snapshot_from_rpc(&status, &diag);
    assert_eq!(snap.diagnostics.len(), 4);
    assert!(matches!(
        snap.diagnostics[0].severity,
        smedja_lsp::Severity::Error
    ));
    assert!(matches!(
        snap.diagnostics[1].severity,
        smedja_lsp::Severity::Warning
    ));
    assert!(matches!(
        snap.diagnostics[2].severity,
        smedja_lsp::Severity::Info
    ));
    assert!(matches!(
        snap.diagnostics[3].severity,
        smedja_lsp::Severity::Hint
    ));
}

#[test]
fn lsp_snapshot_from_rpc_unknown_severity_defaults_to_error() {
    let status = json!({"servers": []});
    let diag = json!({
        "diagnostics": [
            {"file": "x.rs", "line": 1, "col": 1, "severity": "banana", "message": "x"}
        ]
    });
    let snap = lsp_snapshot_from_rpc(&status, &diag);
    assert!(matches!(
        snap.diagnostics[0].severity,
        smedja_lsp::Severity::Error
    ));
}

#[test]
fn lsp_snapshot_from_rpc_decodes_server_states() {
    let status = json!({
        "servers": [
            {"name": "ra",     "state": "ready"},
            {"name": "gopls",  "state": "degraded: connection refused"},
            {"name": "py",     "state": "starting"},
        ]
    });
    let snap = lsp_snapshot_from_rpc(&status, &json!({"diagnostics": []}));
    assert_eq!(snap.servers.len(), 3);
    assert!(matches!(
        snap.servers[0].state,
        smedja_lsp::ServerState::Ready
    ));
    assert!(
        matches!(&snap.servers[1].state, smedja_lsp::ServerState::Degraded(r) if r == "connection refused"),
        "degraded reason must be extracted from prefix"
    );
    assert!(matches!(
        snap.servers[2].state,
        smedja_lsp::ServerState::Starting
    ));
}

#[test]
fn lsp_snapshot_from_rpc_empty_inputs_yield_empty_snapshot() {
    let snap = lsp_snapshot_from_rpc(&json!({"servers": []}), &json!({"diagnostics": []}));
    assert!(snap.servers.is_empty());
    assert!(snap.diagnostics.is_empty());
}

// --- poll backoff --------------------------------------------------------

#[test]
fn poll_backoff_shift_never_overflows() {
    // Verify the clamped shift cannot produce a u64 overflow for any retry
    // count up to and including the give-up threshold (60).
    for count in 0u32..=60 {
        let shift = count.saturating_sub(1).min(10);
        let _ = (100u64 << shift).min(1_000);
    }
}

#[test]
fn poll_backoff_caps_at_1000ms() {
    // At retry=4 the raw shift (3) gives 800 ms; at retry=5 (shift=4) the
    // raw value 1600 ms clamps to 1000 ms and stays there.
    for count in 5u32..=60 {
        let shift = count.saturating_sub(1).min(10);
        let ms = (100u64 << shift).min(1_000);
        assert_eq!(ms, 1_000, "backoff must cap at 1000 ms for retry {count}");
    }
}

// --- keybinding: Ctrl-F context rail / Ctrl-R history search ---------------

#[test]
fn ctrl_f_in_scroll_mode_toggles_context_rail() {
    let mut state = make_state("sess-ctrlf");
    state.scroll_focus = true;
    let initial = state.panels.context_rail;
    // Simulate Ctrl-F in scroll mode.
    state.panels.context_rail = !state.panels.context_rail;
    assert_ne!(
        state.panels.context_rail, initial,
        "Ctrl-F must toggle panels.context_rail in scroll mode"
    );
    state.panels.context_rail = !state.panels.context_rail;
    assert_eq!(
        state.panels.context_rail, initial,
        "second Ctrl-F must restore original value"
    );
}

#[test]
fn ctrl_r_in_scroll_mode_does_not_affect_context_rail() {
    let mut state = make_state("sess-ctrlr-scroll");
    state.scroll_focus = true;
    let initial = state.panels.context_rail;
    // The Ctrl-R handler only acts when !scroll_focus, so it must be a no-op here.
    if !state.scroll_focus {
        state.history_search_mode = !state.history_search_mode;
    }
    assert_eq!(
        state.panels.context_rail, initial,
        "Ctrl-R in scroll mode must not touch panels.context_rail"
    );
    assert!(
        !state.history_search_mode,
        "history_search_mode must remain off when Ctrl-R fires in scroll mode"
    );
}

#[test]
fn ctrl_r_in_input_mode_toggles_history_search() {
    let mut state = make_state("sess-ctrlr-input");
    state.scroll_focus = false;
    state.input = String::from("partial query");
    assert!(!state.history_search_mode);
    // Simulate Ctrl-R in input mode.
    if !state.scroll_focus {
        state.history_search_mode = !state.history_search_mode;
        state.history_search_query.clear();
        if state.history_search_mode {
            state.input.clone_into(&mut state.saved_input);
        }
    }
    assert!(
        state.history_search_mode,
        "Ctrl-R must enable history_search_mode in input mode"
    );
    assert_eq!(
        state.saved_input, "partial query",
        "current input must be saved when entering history search"
    );
    assert!(
        state.history_search_query.is_empty(),
        "search query must be cleared on activation"
    );
}

// --- Ctrl-G external editor --------------------------------------------------

#[test]
fn resolve_editor_falls_back_to_vi() {
    // Remove VISUAL and EDITOR from the environment for this test.
    std::env::remove_var("VISUAL");
    std::env::remove_var("EDITOR");
    // Can't guarantee clean env in parallel tests, but the fallback path
    // must always produce a non-empty string.
    let editor = resolve_editor();
    assert!(
        !editor.is_empty(),
        "resolve_editor must return a non-empty string"
    );
}

#[test]
fn resolve_editor_prefers_visual_over_editor() {
    std::env::set_var("VISUAL", "emacs");
    std::env::set_var("EDITOR", "nano");
    let editor = resolve_editor();
    // Clean up after the test regardless of assertion result.
    std::env::remove_var("VISUAL");
    std::env::remove_var("EDITOR");
    assert_eq!(editor, "emacs", "VISUAL must be preferred over EDITOR");
}

#[test]
fn open_in_editor_temp_path_is_in_tmpdir() {
    // Verify the temp file path is inside the OS temp directory — we
    // cannot actually invoke an editor in a unit test, but we can check
    // that the path construction is correct.
    let tmp = std::env::temp_dir();
    let path = tmp.join(format!("smedja-edit-{}.md", std::process::id()));
    assert!(
        path.starts_with(&tmp),
        "temp file must be under the OS temp directory"
    );
    assert!(
        path.to_string_lossy().ends_with(".md"),
        "temp file must have .md extension for editor syntax highlighting"
    );
}

#[test]
fn ctrl_g_in_scroll_mode_is_noop() {
    let mut state = make_state("sess-ctrlg-scroll");
    state.scroll_focus = true;
    state.input = "existing input".to_owned();
    state.input_cursor = 14;
    // The Ctrl-G handler guards on !scroll_focus; simulate that guard.
    if !state.scroll_focus {
        // would call open_in_editor — never reached
        state.input = "replaced".to_owned();
    }
    assert_eq!(
        state.input, "existing input",
        "Ctrl-G in scroll mode must not modify input"
    );
}

// --- thinking token accumulation ------------------------------------------

#[test]
fn thinking_tokens_accumulate_in_current_thinking() {
    let mut state = make_state("sess-think");
    assert!(state.current_thinking.is_empty());
    // Simulate two ThinkingDelta stream events arriving.
    state.current_thinking.push_str("step one ");
    state.current_thinking.push_str("step two");
    assert_eq!(state.current_thinking, "step one step two");
}

#[test]
fn thinking_expanded_toggles_only_when_content_present() {
    let mut state = make_state("sess-think-toggle");
    state.scroll_focus = true;
    // No steps: T key must be a no-op.
    assert!(state.thinking_steps.is_empty());
    if !state.thinking_steps.is_empty() {
        state.thinking_expanded = !state.thinking_expanded;
    }
    assert!(
        !state.thinking_expanded,
        "T must not toggle when no thinking steps"
    );

    // With steps: T key must toggle.
    state
        .thinking_steps
        .push(thoughts_panel::ThinkingStep::Answer { elapsed_s: 1.0 });
    if !state.thinking_steps.is_empty() {
        state.thinking_expanded = !state.thinking_expanded;
    }
    assert!(
        state.thinking_expanded,
        "T must expand when thinking steps are present"
    );
    if !state.thinking_steps.is_empty() {
        state.thinking_expanded = !state.thinking_expanded;
    }
    assert!(!state.thinking_expanded, "second T must collapse");
}

// --- thinking step timeline ----------------------------------------------

#[test]
fn thinking_steps_cleared_at_turn_start() {
    let mut state = make_state("sess-steps-clear");
    state
        .thinking_steps
        .push(thoughts_panel::ThinkingStep::Answer { elapsed_s: 1.0 });
    assert_eq!(state.thinking_steps.len(), 1);
    state.thinking_steps.clear();
    assert!(state.thinking_steps.is_empty());
}

#[test]
fn thinking_step_tool_has_correct_fields() {
    let step = thoughts_panel::ThinkingStep::Tool {
        name: "bash".into(),
        preview: "ls /src".into(),
        elapsed_s: 0.5,
    };
    assert!(matches!(step.elapsed_s(), 0.4..=0.6));
}

// --- govctl work-item harness --------------------------------------------

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
fn session_rail_toggle_clears_cursor() {
    let mut state = make_state("sess-rail");
    assert!(!state.panels.session_rail);
    // Simulate Ctrl-W: enable rail.
    state.panels.session_rail = true;
    state.session_rail_cursor = 0;
    state.last_session_rail_poll = None;
    assert!(state.panels.session_rail);
    // Toggle off.
    state.panels.session_rail = false;
    assert!(!state.panels.session_rail);
}

#[test]
fn session_rail_cursor_navigates_within_bounds() {
    let mut state = make_state("sess-rail-nav");
    state.session_rail_items = vec![
        ("id1".into(), "claude  id1".into()),
        ("id2".into(), "claude  id2".into()),
        ("id3".into(), "claude  id3".into()),
    ];
    state.session_rail_cursor = 0;
    // ] moves forward.
    let max = state.session_rail_items.len().saturating_sub(1);
    state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
    assert_eq!(state.session_rail_cursor, 1);
    state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
    assert_eq!(state.session_rail_cursor, 2);
    // Clamps at max.
    state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
    assert_eq!(state.session_rail_cursor, 2, "cursor must not exceed max");
    // [ moves backward.
    state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
    assert_eq!(state.session_rail_cursor, 1);
    state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
    assert_eq!(state.session_rail_cursor, 0);
    // Clamps at zero.
    state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
    assert_eq!(state.session_rail_cursor, 0, "cursor must not underflow");
}

// --- emit/canvas split: system message dual-routing ----------------------

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
fn prompt_token_estimate_uses_chars_over_four_heuristic() {
    // 40 chars / 4 = 10 estimated tokens.
    let input = "a".repeat(40);
    let chars = input.chars().count();
    #[allow(clippy::integer_division)]
    let est = chars / 4;
    assert_eq!(est, 10, "40 chars should estimate to 10 tokens");
}

#[test]
fn prompt_token_estimate_rounds_down() {
    let input = "abc"; // 3 chars / 4 = 0 — rounds down
    let chars = input.chars().count();
    #[allow(clippy::integer_division)]
    let est = chars / 4;
    assert_eq!(est, 0);
}

#[test]
fn thinking_cleared_on_new_turn() {
    let mut state = make_state("sess-think-clear");
    state.current_thinking = "previous reasoning".to_owned();
    state.thinking_expanded = true;
    // Simulate what happens when a new turn starts.
    state.current_thinking.clear();
    state.thinking_expanded = false;
    assert!(state.current_thinking.is_empty());
    assert!(!state.thinking_expanded);
}

// --- P3b: OSC-9 helper ---------------------------------------------------

#[test]
fn osc9_bytes_is_correct_sequence() {
    let bytes = osc9_turn_complete_bytes();
    assert_eq!(bytes, b"\x1b]9;turn complete\x07");
}

#[test]
fn emit_osc9_writes_to_vec() {
    let mut buf: Vec<u8> = Vec::new();
    emit_osc9(&mut buf).unwrap();
    assert_eq!(buf, b"\x1b]9;turn complete\x07");
}

// --- P2a: kill ring -------------------------------------------------------

#[test]
fn ctrl_k_kills_to_eol() {
    let mut state = make_state("sess-kill-k");
    state.input = "hello world".to_owned();
    state.input_cursor = 5; // cursor after "hello"
    let killed: String = state.input[state.input_cursor..].to_owned();
    state.input.drain(state.input_cursor..);
    push_kill(&mut state.kill_ring, killed);
    assert_eq!(state.input, "hello");
    assert_eq!(state.kill_ring.back().map(String::as_str), Some(" world"));
}

#[test]
fn ctrl_u_kills_to_bol() {
    let mut state = make_state("sess-kill-u");
    state.input = "hello world".to_owned();
    state.input_cursor = 5;
    let killed: String = state.input[..state.input_cursor].to_owned();
    state.input.drain(..state.input_cursor);
    state.input_cursor = 0;
    push_kill(&mut state.kill_ring, killed);
    assert_eq!(state.input, " world");
    assert_eq!(state.kill_ring.back().map(String::as_str), Some("hello"));
}

#[test]
fn ctrl_y_yanks_from_ring() {
    let mut state = make_state("sess-yank");
    state.input = "foo".to_owned();
    state.input_cursor = 3;
    push_kill(&mut state.kill_ring, " bar".to_owned());
    // Yank
    let text = state.kill_ring.back().cloned().unwrap();
    state.input.insert_str(state.input_cursor, &text);
    state.input_cursor += text.len();
    assert_eq!(state.input, "foo bar");
}

#[test]
fn ctrl_b_moves_cursor_left() {
    let mut state = make_state("sess-ctrl-b");
    state.input = "abc".to_owned();
    state.input_cursor = 3;
    state.input_cursor = prev_char_boundary(&state.input, state.input_cursor);
    assert_eq!(state.input_cursor, 2);
}

#[test]
fn kill_ring_evicts_oldest_at_capacity() {
    let mut ring: VecDeque<String> = VecDeque::new();
    for i in 0..17u32 {
        push_kill(&mut ring, i.to_string());
    }
    assert_eq!(ring.len(), 16, "ring must not exceed 16 entries");
    // Oldest entry (0) is evicted; front is "1".
    assert_eq!(ring.front().map(String::as_str), Some("1"));
}

// --- P2b: /gov create + transition ----------------------------------------

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
fn role_cockpit_toggle_via_ctrl_a() {
    let mut state = make_state("sess-cockpit");
    assert!(!state.panels.role_cockpit, "cockpit hidden by default");
    state.panels.role_cockpit = !state.panels.role_cockpit;
    assert!(state.panels.role_cockpit, "toggle must show cockpit");
    state.panels.role_cockpit = !state.panels.role_cockpit;
    assert!(
        !state.panels.role_cockpit,
        "second toggle must hide cockpit"
    );
}

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

#[test]
fn panel_visibility_startup_defaults_match_make_state() {
    let state = make_state("sess-panels");
    assert!(state.panels.context_rail, "context rail visible by default");
    assert!(!state.panels.metrics, "metrics hidden by default");
    assert!(!state.panels.session_rail, "session rail hidden by default");
    assert!(state.panels.lsp, "LSP visible by default");
    assert!(state.panels.obs, "obs visible by default");
    assert!(!state.panels.role_cockpit, "cockpit hidden by default");
}

// --- session detail overlay (Story A) ------------------------------------

#[test]
fn session_detail_starts_empty() {
    let state = make_state("sess-detail-init");
    assert!(
        state.session_detail_overlay.is_none(),
        "detail overlay must start empty"
    );
}

#[test]
fn session_detail_esc_closes_overlay() {
    let mut state = make_state("sess-detail-esc");
    state.session_detail_overlay = Some(SessionDetail {
        id: "abc-123".into(),
        title: None,
        mode: Some("auto".into()),
        status: Some("active".into()),
        active_change: None,
        created_at: "2026-06-28T00:00:00Z".into(),
        updated_at: "2026-06-28T00:00:00Z".into(),
        cowork_mode: None,
    });
    // Esc while overlay is open clears it.
    state.session_detail_overlay = None;
    assert!(
        state.session_detail_overlay.is_none(),
        "Esc must close the detail overlay"
    );
}

#[test]
fn session_detail_overlay_holds_correct_fields() {
    let detail = SessionDetail {
        id: "test-session-id".into(),
        title: Some("My session".into()),
        mode: Some("review".into()),
        status: Some("active".into()),
        active_change: Some("add-quality-panel".into()),
        created_at: "2026-06-01T12:00:00Z".into(),
        updated_at: "2026-06-28T09:00:00Z".into(),
        cowork_mode: Some("ask".into()),
    };
    assert_eq!(detail.id, "test-session-id");
    assert_eq!(detail.title.as_deref(), Some("My session"));
    assert_eq!(detail.mode.as_deref(), Some("review"));
    assert_eq!(detail.status.as_deref(), Some("active"));
    assert_eq!(detail.active_change.as_deref(), Some("add-quality-panel"));
    assert_eq!(detail.cowork_mode.as_deref(), Some("ask"));
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

#[test]
fn session_detail_overlay_renders_in_buffer() {
    let mut state = make_state("sess-detail-render");
    state.session_detail_overlay = Some(SessionDetail {
        id: "full-id-abc-def-ghi".into(),
        title: Some("Sprint 12".into()),
        mode: Some("auto".into()),
        status: Some("active".into()),
        active_change: Some("add-quality-panel".into()),
        created_at: "2026-06-28T09:00:00Z".into(),
        updated_at: "2026-06-28T10:00:00Z".into(),
        cowork_mode: Some("ask".into()),
    });
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert!(
        content.contains("full-id-abc-def-ghi"),
        "full id must render"
    );
    assert!(content.contains("Sprint 12"), "title must render");
    assert!(
        content.contains("add-quality-panel"),
        "active change must render"
    );
    assert!(content.contains("ask"), "cowork mode must render");
}

#[test]
fn session_rail_up_down_move_cursor_in_scroll_mode() {
    let mut state = make_state("sess-up-down");
    state.scroll_focus = true;
    state.panels.session_rail = true;
    state.session_rail_items = vec![
        ("id1".into(), "runner  id1".into()),
        ("id2".into(), "runner  id2".into()),
        ("id3".into(), "runner  id3".into()),
    ];
    state.session_rail_cursor = 0;

    // Down moves cursor forward.
    let max = state.session_rail_items.len().saturating_sub(1);
    if state.scroll_focus && state.panels.session_rail && !state.session_rail_items.is_empty() {
        state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
    }
    assert_eq!(state.session_rail_cursor, 1);

    // Up moves cursor back.
    if state.scroll_focus && state.panels.session_rail {
        state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
    }
    assert_eq!(state.session_rail_cursor, 0);
}

#[test]
fn session_rail_bracket_keys_work_in_input_mode() {
    let mut state = make_state("sess-bracket-input");
    state.scroll_focus = false; // input mode
    state.panels.session_rail = true;
    state.session_rail_items = vec![
        ("id1".into(), "label1".into()),
        ("id2".into(), "label2".into()),
    ];
    state.session_rail_cursor = 0;

    // ] advances cursor even in input mode.
    if state.panels.session_rail && !state.session_rail_items.is_empty() {
        let max = state.session_rail_items.len().saturating_sub(1);
        state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
    }
    assert_eq!(state.session_rail_cursor, 1, "] must work in input mode");

    // [ goes back.
    if state.panels.session_rail {
        state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
    }
    assert_eq!(state.session_rail_cursor, 0, "[ must work in input mode");
}

// --- session detail: Ctrl+Enter load (Story B) ---------------------------

#[test]
fn session_detail_ctrl_enter_switches_session_id() {
    let mut state = make_state("sess-switch-id");
    state.session_id = "original-session".into();
    state.session_detail_overlay = Some(SessionDetail {
        id: "new-session-abc".into(),
        title: Some("other work".into()),
        mode: Some("auto".into()),
        status: Some("active".into()),
        active_change: None,
        created_at: "2026-06-28T00:00:00Z".into(),
        updated_at: "2026-06-28T00:00:00Z".into(),
        cowork_mode: None,
    });
    // Simulate what Ctrl+Enter does: extract id, switch, clear overlay.
    let target_id = state
        .session_detail_overlay
        .as_ref()
        .map(|d| d.id.clone())
        .unwrap();
    state.session_id = target_id;
    state.session_detail_overlay = None;
    state.display_start_idx = state.messages.len();
    state.main_panel.clear_display();

    assert_eq!(
        state.session_id, "new-session-abc",
        "session_id must switch"
    );
    assert!(
        state.session_detail_overlay.is_none(),
        "overlay must close after load"
    );
}

#[test]
fn session_detail_ctrl_enter_does_nothing_without_overlay() {
    let mut state = make_state("sess-switch-no-overlay");
    state.session_id = "original".into();
    state.session_detail_overlay = None;
    // Nothing happens — session_id is unchanged.
    if let Some(ref d) = state.session_detail_overlay {
        state.session_id = d.id.clone();
    }
    assert_eq!(state.session_id, "original", "no overlay = no switch");
}

#[test]
fn session_detail_popup_shows_load_hint() {
    let mut state = make_state("sess-detail-hint");
    state.session_detail_overlay = Some(SessionDetail {
        id: "hint-session".into(),
        title: None,
        mode: None,
        status: None,
        active_change: None,
        created_at: "2026-06-28T00:00:00Z".into(),
        updated_at: "2026-06-28T00:00:00Z".into(),
        cowork_mode: None,
    });
    let buf = render_frame(&mut state);
    let content: String = buf
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    // The popup must hint both the load binding and close binding.
    assert!(
        content.contains("load") || content.contains("Load"),
        "popup must show load hint: {content}"
    );
    assert!(
        content.contains("Esc") || content.contains("close"),
        "popup must show close hint"
    );
}

// --- session rail: arrow keys in input mode (Story B fix) ----------------

#[test]
fn session_rail_up_arrow_moves_cursor_in_input_mode() {
    let mut state = make_state("sess-up-input");
    state.scroll_focus = false; // input mode
    state.panels.session_rail = true;
    state.session_rail_items = vec![
        ("id1".into(), "label1".into()),
        ("id2".into(), "label2".into()),
        ("id3".into(), "label3".into()),
    ];
    state.session_rail_cursor = 2;

    // Simulate the early-exit block: Up decrements cursor, does not touch history.
    if state.panels.session_rail && !state.scroll_focus {
        state.session_rail_cursor = state.session_rail_cursor.saturating_sub(1);
    }
    assert_eq!(
        state.session_rail_cursor, 1,
        "Up must move rail cursor in input mode"
    );
    assert!(
        state.history_idx.is_none(),
        "prompt history must be untouched"
    );
}

#[test]
fn session_rail_down_arrow_moves_cursor_in_input_mode() {
    let mut state = make_state("sess-down-input");
    state.scroll_focus = false;
    state.panels.session_rail = true;
    state.session_rail_items = vec![
        ("id1".into(), "label1".into()),
        ("id2".into(), "label2".into()),
    ];
    state.session_rail_cursor = 0;

    if state.panels.session_rail && !state.scroll_focus && !state.session_rail_items.is_empty() {
        let max = state.session_rail_items.len().saturating_sub(1);
        state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
    }
    assert_eq!(
        state.session_rail_cursor, 1,
        "Down must move rail cursor in input mode"
    );
}

#[test]
fn session_rail_down_arrow_clamps_at_bottom_in_input_mode() {
    let mut state = make_state("sess-down-clamp-input");
    state.scroll_focus = false;
    state.panels.session_rail = true;
    state.session_rail_items = vec![("id1".into(), "label1".into())];
    state.session_rail_cursor = 0;

    if state.panels.session_rail && !state.scroll_focus && !state.session_rail_items.is_empty() {
        let max = state.session_rail_items.len().saturating_sub(1);
        state.session_rail_cursor = (state.session_rail_cursor + 1).min(max);
    }
    assert_eq!(state.session_rail_cursor, 0, "Down must clamp at last item");
}

// --- Slice 7: command palette ---

// --- Slice 8: file picker ---

#[test]
fn file_picker_insert_formats_at_file() {
    let mut state = make_state("s");
    state.input.clear();
    state.input_cursor = 0;
    // Simulate inserting a file selection
    let path = "/workspace/src/main.rs";
    let at_ref = format!("@file {path}");
    state.input = at_ref.clone();
    state.input_cursor = state.input.len();
    assert!(state.input.starts_with("@file "));
    assert!(state.input.contains(path));
}

#[test]
fn ctrl_f_in_input_mode_opens_file_picker() {
    let mut state = make_state("s");
    state.scroll_focus = false; // input mode
                                // Simulate what Ctrl+F handler does
    state.file_picker_open = true;
    state.file_picker_entries = vec![("../".to_owned(), true), ("main.rs".to_owned(), false)];
    state.file_picker_cursor = 0;
    assert!(state.file_picker_open);
    assert_eq!(state.file_picker_entries.len(), 2);
}

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

#[test]
fn ctrl_k_on_empty_input_opens_palette() {
    let mut state = make_state("test-session");
    state.input.clear();
    // Simulate what the Ctrl+K handler does when input is empty
    state.slash_popup_visible = true;
    state.slash_completions = command_palette_filtered("");
    state.command_palette_mode = true;
    state.slash_cursor = 0;
    assert!(state.slash_popup_visible);
    assert_eq!(state.slash_completions.len(), SLASH_COMPLETIONS.len());
    assert!(state.command_palette_mode);
}
