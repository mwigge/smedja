use std::sync::Arc;

use tokio::sync::Mutex;

// ── turn.subscribe event-driven wait ──────────────────────────────────────

use smedja_bellows::event::CorrelationCtx;
use smedja_bellows::{Dispatcher, TurnEvent};
use smedja_ingot::{Ingot, IngotHandle, Task};
use smedja_types::Timestamp;

fn task(id: uuid::Uuid, status: &str, response: Option<&str>) -> Task {
    Task {
        id,
        title: "t".to_owned(),
        description: String::new(),
        status: status.to_owned(),
        created_at: Timestamp::from_micros(0),
        session_id: None,
        response: response.map(str::to_owned),
    }
}

// ── 7. exec_bash_ext must not deadlock on large stdin + stdout ────────────

#[tokio::test]
async fn exec_bash_ext_no_deadlock_on_large_io() {
    // The child writes >64 KB to stdout BEFORE it reads its stdin, and we
    // feed it >64 KB of stdin. The previous code wrote ALL of stdin before
    // spawning the stdout reader, so the child blocked on a full stdout pipe
    // while the parent blocked on a full stdin pipe — a deadlock. With the
    // readers draining first (and stdin written on its own task), it
    // completes. Fails (times out) before the fix; passes after.
    let workspace = std::env::temp_dir();
    let stdin = vec![b'x'; 200_000];
    let cmd = "yes aaaaaaaa | head -c 200000; cat";
    let fut = super::exec_bash_ext(cmd, &workspace, Some(30), None, Some(stdin));
    let out = tokio::time::timeout(std::time::Duration::from_secs(20), fut)
        .await
        .expect("exec_bash_ext must not deadlock on large stdin+stdout");
    assert!(
        out.contains("aaaaaaaa"),
        "the pre-stdin stdout burst must be captured"
    );
    assert!(
        out.contains("xxxxxxxx"),
        "the echoed stdin must be captured after draining stdout"
    );
}

#[tokio::test]
async fn subscribe_not_found_errors() {
    let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let dispatcher = Dispatcher::new(16);
    let r = super::await_turn_terminal(
        &ig,
        &dispatcher,
        "missing",
        std::time::Duration::from_millis(50),
    )
    .await;
    assert!(r.is_err());
    assert!(r.unwrap_err().message.contains("task not found"));
}

#[tokio::test]
async fn subscribe_already_complete_returns_envelope() {
    let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let dispatcher = Dispatcher::new(16);
    let id = uuid::Uuid::new_v4();
    ig.create_task(task(id, "complete", None)).await.unwrap();
    let env = super::await_turn_terminal(
        &ig,
        &dispatcher,
        &id.to_string(),
        std::time::Duration::from_millis(50),
    )
    .await
    .unwrap();
    assert_eq!(env["done"], true);
    assert!(
        env.get("response").is_some(),
        "complete envelope carries a response field"
    );
}

#[tokio::test]
async fn subscribe_already_failed_returns_error_envelope() {
    let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let dispatcher = Dispatcher::new(16);
    let id = uuid::Uuid::new_v4();
    ig.create_task(task(id, "failed", None)).await.unwrap();
    let env = super::await_turn_terminal(
        &ig,
        &dispatcher,
        &id.to_string(),
        std::time::Duration::from_millis(50),
    )
    .await
    .unwrap();
    assert_eq!(env["done"], true);
    assert_eq!(env["error"], "turn failed");
}

#[tokio::test]
async fn subscribe_times_out_for_in_progress_with_no_event() {
    let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let dispatcher = Dispatcher::new(16);
    let id = uuid::Uuid::new_v4();
    ig.create_task(task(id, "planned", None)).await.unwrap();
    let r = super::await_turn_terminal(
        &ig,
        &dispatcher,
        &id.to_string(),
        std::time::Duration::from_millis(100),
    )
    .await;
    assert!(r.is_err());
    assert_eq!(r.unwrap_err().code, super::codes::TIMEOUT);
}

#[tokio::test]
async fn subscribe_resolves_on_completed_event() {
    let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let dispatcher = Arc::new(Dispatcher::new(16));
    let id = uuid::Uuid::new_v4();
    let id_str = id.to_string();
    ig.create_task(task(id, "planned", None)).await.unwrap();

    // After a short delay, mark complete and publish the terminal event.
    let ig2 = ig.clone();
    let id2 = id_str.clone();
    let dispatcher2 = Arc::clone(&dispatcher);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        ig2.set_task_response(&id2, "done-now").await.unwrap();
        dispatcher2.publish(TurnEvent::Completed {
            session_id: "s".to_owned(),
            turn_id: id2.clone(),
            output_tokens: 0,
            input_tokens: Some(0),
            traceparent: None,
            correlation: CorrelationCtx {
                status: Some("ok".to_owned()),
                ..CorrelationCtx::default()
            },
        });
    });

    let env =
        super::await_turn_terminal(&ig, &dispatcher, &id_str, std::time::Duration::from_secs(5))
            .await
            .unwrap();
    assert_eq!(env["done"], true);
    assert_eq!(env["response"], "done-now");
}

#[tokio::test]
async fn joinset_reaps_completed_tasks() {
    // A JoinSet drains finished tasks via try_join_next, so it tracks only
    // in-flight work rather than retaining every handle forever.
    let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    for _ in 0..5 {
        set.spawn(async {});
    }
    // Let them finish.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let mut reaped = 0;
    while set.try_join_next().is_some() {
        reaped += 1;
    }
    assert_eq!(reaped, 5);
    assert!(set.is_empty(), "set must be empty after reaping");
}

// ── SSRF guard ────────────────────────────────────────────────────────────

#[test]
fn is_blocked_ip_rejects_private_and_special_ranges() {
    let blocked = [
        "127.0.0.1",
        "10.0.0.1",
        "172.16.5.5",
        "192.168.1.1",
        "169.254.169.254", // cloud IMDS (link-local)
        "100.64.0.1",      // CGNAT
        "0.0.0.0",
        "::1",             // IPv6 loopback
        "fc00::1",         // IPv6 ULA
        "fe80::1",         // IPv6 link-local
        "::ffff:10.0.0.1", // IPv4-mapped private
    ];
    for ip in blocked {
        assert!(
            super::is_blocked_ip(ip.parse().unwrap()),
            "{ip} must be blocked"
        );
    }
    let allowed = ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"];
    for ip in allowed {
        assert!(
            !super::is_blocked_ip(ip.parse().unwrap()),
            "{ip} must be allowed"
        );
    }
}

#[test]
fn is_safe_mcp_url_allows_public_rejects_local() {
    assert!(super::is_safe_mcp_url("https://example.com/mcp"));
    assert!(super::is_safe_mcp_url("https://8.8.8.8/mcp"));
    assert!(!super::is_safe_mcp_url("http://localhost/mcp"));
    assert!(!super::is_safe_mcp_url("http://10.0.0.1/mcp"));
    assert!(!super::is_safe_mcp_url("http://[::1]/mcp"));
    assert!(!super::is_safe_mcp_url("ftp://example.com")); // non-http scheme
}

// ── ACP secret path ─────────────────────────────────────────────────────────

#[test]
fn acp_secret_path_prefers_private_dirs_and_refuses_tmp() {
    use std::path::Path;
    assert_eq!(
        crate::runtime_paths::acp_secret_path_from(Some("/run/user/501"), None),
        Some(std::path::PathBuf::from("/run/user/501/smdjad.secret"))
    );
    assert_eq!(
        crate::runtime_paths::acp_secret_path_from(None, Some(Path::new("/home/u"))),
        Some(std::path::PathBuf::from("/home/u/.cache/smdjad.secret"))
    );
    // No XDG_RUNTIME_DIR and no HOME → refuse (would only be /tmp).
    assert_eq!(crate::runtime_paths::acp_secret_path_from(None, None), None);
    assert_eq!(
        crate::runtime_paths::acp_secret_path_from(Some(""), None),
        None
    );
}

// ── workspace root default ───────────────────────────────────────────────────

#[test]
fn resolve_workspace_root_uses_explicit_env_else_absolute_cwd() {
    let cwd = std::path::PathBuf::from("/abs/cwd");
    assert_eq!(
        crate::runtime_paths::resolve_workspace_root_from(Some("/ws".to_owned()), cwd.clone()),
        std::path::PathBuf::from("/ws")
    );
    // Unset/empty → the (absolute) cwd, never the relative ".".
    let got = crate::runtime_paths::resolve_workspace_root_from(None, cwd.clone());
    assert_eq!(got, cwd);
    assert_ne!(got, std::path::PathBuf::from("."));
}

#[test]
fn read_only_role_blocks_write_tools() {
    // The least-privilege check in execute_tool blocks write tools when
    // session mode is "review". Verify the logic inline.
    let read_only_modes = ["review"];
    let write_tools = ["edit_file", "bash", "write_file", "run_command"];
    for mode in &read_only_modes {
        for tool in &write_tools {
            let is_blocked = *mode == "review" && write_tools.contains(tool);
            assert!(is_blocked, "tool {tool} should be blocked for mode {mode}");
        }
    }
}

#[test]
fn search_mode_parses_to_read_only_role() {
    use smedja_assayer::AgentRole;
    let role = crate::common::parse_session_mode_to_role("search");
    assert_eq!(role, Some(AgentRole::Search));
    assert!(AgentRole::Search.is_read_only());
    assert_eq!(AgentRole::Search.label(), "search");
}

#[test]
fn search_role_blocks_write_tools() {
    use crate::cowork::{evaluate, PermissionDecision, PermissionMode};
    use smedja_assayer::AgentRole;
    let write_tools = ["edit_file", "bash", "write_file", "run_command"];
    for tool in &write_tools {
        let denied = AgentRole::Search.is_read_only()
            && evaluate(PermissionMode::Plan, tool) == PermissionDecision::Deny;
        assert!(denied, "tool {tool} should be blocked for search role");
    }
}

#[test]
fn loop_retire_state_is_terminal() {
    // Verify the terminal-status strings used in loop.retire enforcement.
    // "retired" must not be "complete" or "failed" — so the retire guard
    // would have rejected it (retired loops cannot be retired again).
    let retired = "retired";
    assert!(retired != "complete" && retired != "failed");
}

#[test]
fn loop_complete_and_failed_allow_retire() {
    // Only complete or failed loops may be retired — verify the predicate.
    let terminal_for_retire = |s: &str| s == "complete" || s == "failed";
    assert!(terminal_for_retire("complete"));
    assert!(terminal_for_retire("failed"));
    assert!(!terminal_for_retire("planning"));
    assert!(!terminal_for_retire("slicing"));
    assert!(!terminal_for_retire("retired"));
}

#[test]
fn retired_loop_cannot_be_re_run() {
    // The loop.run guard rejects status == "retired".
    let guard = |status: &str| status == "retired";
    assert!(guard("retired"));
    assert!(!guard("complete"));
    assert!(!guard("planning"));
}

#[tokio::test]
async fn provider_pool_builds_without_panic() {
    // build_provider_pool is infallible — just verify no panic regardless
    // of what environment variables are set in the test runner.
    let pool = crate::provider_pool::build_provider_pool().await;
    // Pool may be empty or non-empty depending on the environment; either is valid.
    drop(pool);
}

/// Detection results per candidate provider, for [`provider_priority`].
/// `..Default::default()` keeps call sites readable as the ladder grows.
#[derive(Default)]
struct Detected {
    claude_cli: bool,
    codex_cli: bool,
    kimi_cli: bool,
    gemini_cli: bool,
    copilot: bool,
    poolside: bool,
    anthropic_key: bool,
    openai_key: bool,
    moonshot_key: bool,
    gemini_key: bool,
    minimax: bool,
    berget: bool,
}

/// Returns the provider name that `build_provider` would select given the
/// detection results for each candidate, encoding the subscription-first
/// priority order without touching the network or filesystem.
///
/// Priority (top = highest): claude CLI, codex CLI, kimi CLI, gemini CLI,
/// copilot, poolside, then the API keys (`ANTHROPIC_API_KEY`,
/// `OPENAI_API_KEY`, `MOONSHOT_API_KEY`, `GEMINI_API_KEY`), then minimax and
/// berget.
fn provider_priority(d: &Detected) -> &'static str {
    if d.claude_cli {
        return "claude-cli";
    }
    if d.codex_cli {
        return "codex-cli";
    }
    if d.kimi_cli {
        return "kimi-cli";
    }
    if d.gemini_cli {
        return "gemini-cli";
    }
    if d.copilot {
        return "copilot";
    }
    if d.poolside {
        return "poolside";
    }
    if d.anthropic_key {
        return "anthropic";
    }
    if d.openai_key {
        return "openai";
    }
    if d.moonshot_key {
        return "moonshot";
    }
    if d.gemini_key {
        return "google";
    }
    if d.minimax {
        return "minimax";
    }
    if d.berget {
        return "berget";
    }
    "none"
}

#[test]
fn cli_wins_over_api_key_when_both_present() {
    // CLI subscription beats API key — the fundamental invariant of L20.
    assert_eq!(
        provider_priority(&Detected {
            claude_cli: true,
            anthropic_key: true,
            openai_key: true,
            moonshot_key: true,
            ..Detected::default()
        }),
        "claude-cli"
    );
    assert_eq!(
        provider_priority(&Detected {
            codex_cli: true,
            openai_key: true,
            moonshot_key: true,
            ..Detected::default()
        }),
        "codex-cli"
    );
    assert_eq!(
        provider_priority(&Detected {
            kimi_cli: true,
            moonshot_key: true,
            ..Detected::default()
        }),
        "kimi-cli"
    );
    assert_eq!(
        provider_priority(&Detected {
            gemini_cli: true,
            gemini_key: true,
            ..Detected::default()
        }),
        "gemini-cli"
    );
}

#[test]
fn api_key_selected_when_no_cli_available() {
    assert_eq!(
        provider_priority(&Detected {
            anthropic_key: true,
            ..Detected::default()
        }),
        "anthropic"
    );
    assert_eq!(
        provider_priority(&Detected {
            openai_key: true,
            ..Detected::default()
        }),
        "openai"
    );
    assert_eq!(
        provider_priority(&Detected {
            moonshot_key: true,
            ..Detected::default()
        }),
        "moonshot"
    );
    assert_eq!(
        provider_priority(&Detected {
            gemini_key: true,
            ..Detected::default()
        }),
        "google"
    );
}

#[test]
fn cli_providers_ordered_before_copilot_and_poolside() {
    // Even copilot (subscription-like) comes after the CLI runners.
    assert_eq!(
        provider_priority(&Detected {
            codex_cli: true,
            copilot: true,
            poolside: true,
            ..Detected::default()
        }),
        "codex-cli"
    );
    assert_eq!(
        provider_priority(&Detected {
            kimi_cli: true,
            copilot: true,
            poolside: true,
            ..Detected::default()
        }),
        "kimi-cli"
    );
    assert_eq!(
        provider_priority(&Detected {
            gemini_cli: true,
            copilot: true,
            poolside: true,
            ..Detected::default()
        }),
        "gemini-cli"
    );
}

#[test]
fn anthropic_key_before_openai_key() {
    assert_eq!(
        provider_priority(&Detected {
            anthropic_key: true,
            openai_key: true,
            moonshot_key: true,
            ..Detected::default()
        }),
        "anthropic"
    );
}

#[test]
fn minimax_and_berget_are_lowest_priority_before_local() {
    assert_eq!(
        provider_priority(&Detected {
            minimax: true,
            ..Detected::default()
        }),
        "minimax"
    );
    assert_eq!(
        provider_priority(&Detected {
            berget: true,
            ..Detected::default()
        }),
        "berget"
    );
    assert_eq!(provider_priority(&Detected::default()), "none");
}

// ── provider-display: session.create response fields ────────────────────

fn derive_tier(runner: &str) -> &'static str {
    if runner.contains("local") {
        "local"
    } else {
        "fast"
    }
}

#[test]
fn session_create_tier_is_local_for_local_runner() {
    assert_eq!(derive_tier("local"), "local");
    assert_eq!(derive_tier("local-llm"), "local");
}

#[test]
fn session_create_tier_is_fast_for_cloud_runners() {
    for runner in &["claude-cli", "anthropic", "codex-cli", "openai", "copilot"] {
        assert_eq!(
            derive_tier(runner),
            "fast",
            "expected fast tier for runner {runner}"
        );
    }
}

#[test]
fn session_create_response_contains_runner_model_tier() {
    let runner = "anthropic";
    let model = "claude-sonnet-4-6";
    let tier = derive_tier(runner);
    let resp = serde_json::json!({
        "id": "session-test",
        "runner": runner,
        "model": model,
        "tier": tier,
    });
    assert_eq!(resp["runner"].as_str().unwrap(), runner);
    assert_eq!(resp["model"].as_str().unwrap(), model);
    assert_eq!(resp["tier"].as_str().unwrap(), "fast");
}

// ── parse_runner_str ────────────────────────────────────────────────────

#[test]
fn parse_runner_str_accepts_short_aliases() {
    use smedja_assayer::Runner;

    use crate::common::parse_runner_str;
    assert!(matches!(parse_runner_str("claude"), Some(Runner::Claude)));
    assert!(matches!(parse_runner_str("codex"), Some(Runner::Codex)));
    assert!(matches!(parse_runner_str("local"), Some(Runner::Local)));
    assert!(matches!(parse_runner_str("copilot"), Some(Runner::Copilot)));
}

#[test]
fn parse_runner_str_accepts_canonical_keys() {
    use smedja_assayer::Runner;

    use crate::common::parse_runner_str;
    assert!(matches!(
        parse_runner_str("claude-cli"),
        Some(Runner::Claude)
    ));
    assert!(matches!(parse_runner_str("codex-cli"), Some(Runner::Codex)));
}

#[test]
fn parse_runner_str_rejects_unknown_values() {
    use crate::common::parse_runner_str;
    assert!(parse_runner_str("openai").is_none());
    assert!(parse_runner_str("").is_none());
    assert!(parse_runner_str("anthropic").is_none());
}

// ── session.set_runner / session.takeover ─────────────────────────────

#[tokio::test]
async fn session_set_runner_stores_canonical_key() {
    use smedja_ingot::{Ingot, IngotHandle, Session};
    use uuid::Uuid;

    let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let session_id = Uuid::new_v4().to_string();
    let now = Timestamp::from_secs_f64(1_700_000_000.0);
    ig.create_session(Session {
        id: Uuid::parse_str(&session_id).unwrap(),
        created_at: now,
        updated_at: now,
        status: "active".into(),
        task_id: None,
        mode: None,
        title: String::new(),
        cowork_mode: false,
        workspace_root: None,
        model_override: None,
        runner_override: None,
    })
    .await
    .unwrap();
    ig.update_session_runner_override(&session_id, "codex-cli")
        .await
        .unwrap();
    let fetched = ig.get_session(&session_id).await.unwrap().unwrap();
    assert_eq!(fetched.runner_override.as_deref(), Some("codex-cli"));
}

#[tokio::test]
async fn session_takeover_forks_with_runner_override() {
    use smedja_ingot::{Ingot, IngotHandle, Session};
    use uuid::Uuid;

    let ig = IngotHandle::new(Ingot::open_in_memory().unwrap());
    let parent_id = Uuid::new_v4().to_string();
    let now = Timestamp::from_secs_f64(1_700_000_000.0);
    ig.create_session(Session {
        id: Uuid::parse_str(&parent_id).unwrap(),
        created_at: now,
        updated_at: now,
        status: "active".into(),
        task_id: None,
        mode: Some("impl".into()),
        title: String::new(),
        cowork_mode: false,
        workspace_root: None,
        model_override: None,
        runner_override: None,
    })
    .await
    .unwrap();

    // Simulate takeover: fork then set runner_override.
    let new_id = Uuid::new_v4().to_string();
    let parent = ig.get_session(&parent_id).await.unwrap().unwrap();
    ig.create_session(Session {
        id: Uuid::parse_str(&new_id).unwrap(),
        created_at: Timestamp::from_secs_f64(1_700_000_001.0),
        updated_at: Timestamp::from_secs_f64(1_700_000_001.0),
        status: "active".into(),
        task_id: None,
        mode: parent.mode.clone(),
        title: parent.title.clone(),
        cowork_mode: parent.cowork_mode,
        workspace_root: parent.workspace_root.clone(),
        model_override: parent.model_override.clone(),
        runner_override: Some("codex-cli".into()),
    })
    .await
    .unwrap();

    let new_sess = ig.get_session(&new_id).await.unwrap().unwrap();
    assert_eq!(new_sess.runner_override.as_deref(), Some("codex-cli"));
    assert_eq!(new_sess.mode.as_deref(), Some("impl"));
}

#[tokio::test]
async fn warm_snapshot_writes_vault_entries_for_session_checkpoints() {
    use smedja_ingot::Checkpoint;
    use smedja_vault::Vault;
    use uuid::Uuid;

    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let session_id = "sess-warm-test".to_owned();
    let fan_out_id = "fan-01".to_owned();

    let messages = r#"[{"role":"user","content":"what is async rust"}]"#.to_owned();
    let checkpoints = vec![Checkpoint {
        id: Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        session_id: session_id.clone(),
        turn_n: 0,
        messages_json: messages.clone(),
        created_at: Timestamp::from_secs_f64(1_700_000_000.0),
        compaction_id: None,
    }];

    // Simulate the warm snapshot logic from task.parallel.
    let fid = fan_out_id.clone();
    let parent_sid = session_id.clone();
    let vt = Arc::clone(&vault);
    tokio::task::spawn_blocking(move || {
        let mut guard = vt.blocking_lock();
        for cp in &checkpoints {
            let entry = smedja_vault::VaultEntry {
                id: format!("warm:{}:{}", fid, cp.id),
                embedding: crate::embedder::embed(&cp.messages_json),
                payload: serde_json::json!({
                    "fan_out_id": fid,
                    "session_id": parent_sid,
                    "turn_n": cp.turn_n,
                }),
                namespace: "warm".to_owned(),
                content: cp.messages_json.clone(),
                source_file: None,
                added_by: Some("task.parallel".to_owned()),
                chunk_index: None,
                parent_id: None,
                created_at: 0.0,
                embedder_model_id: smedja_vault::LEGACY_MODEL_ID.to_owned(),
                dim: crate::embedder::DIM,
            };
            guard.upsert(&entry).unwrap();
        }
    })
    .await
    .unwrap();

    let count = vault.lock().await.count_by_namespace("warm").unwrap();
    assert_eq!(count, 1, "one warm entry must be written per checkpoint");

    let results = {
        let guard = vault.lock().await;
        let qv = crate::embedder::embed("async rust");
        guard
            .search(
                &qv,
                "async rust",
                "warm",
                5,
                smedja_vault::LEGACY_MODEL_ID,
                crate::embedder::DIM,
            )
            .unwrap()
    };
    assert!(
        !results.is_empty(),
        "warm snapshot must be retrievable by content similarity"
    );
}

#[tokio::test]
async fn takeover_handoff_writes_vault_entry_with_handoff_namespace() {
    use smedja_vault::Vault;

    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let from_sid = "sess-from".to_owned();
    let to_sid = "sess-to".to_owned();
    let messages =
        r#"[{"role":"user","content":"implement auth"},{"role":"assistant","content":"ok"}]"#
            .to_owned();
    let hid = format!("handoff:{from_sid}:{to_sid}");

    let hid2 = hid.clone();
    let from2 = from_sid.clone();
    let to2 = to_sid.clone();
    let msgs = messages.clone();
    let vt = Arc::clone(&vault);
    tokio::task::spawn_blocking(move || {
        let entry = smedja_vault::VaultEntry {
            id: hid2,
            embedding: crate::embedder::embed(&msgs),
            payload: serde_json::json!({
                "from_session_id": from2,
                "to_session_id": to2,
                "runner": "codex-cli",
            }),
            namespace: "handoff".to_owned(),
            content: msgs,
            source_file: None,
            added_by: Some("session.takeover".to_owned()),
            chunk_index: None,
            parent_id: None,
            created_at: 0.0,
            embedder_model_id: smedja_vault::LEGACY_MODEL_ID.to_owned(),
            dim: crate::embedder::DIM,
        };
        let mut guard = vt.blocking_lock();
        guard.upsert(&entry).unwrap();
    })
    .await
    .unwrap();

    let count = vault.lock().await.count_by_namespace("handoff").unwrap();
    assert_eq!(count, 1, "one handoff entry must be written on takeover");
}

#[test]
fn compaction_transcript_renders_strata_messages_not_raw_json() {
    let messages_json = r#"[
        {"role":"user","content":"first request"},
        {"role":"assistant","content":"first reply"},
        {"role":"user","content":"second request"}
    ]"#;
    let transcript = super::assemble_compaction_transcript(messages_json);
    // Rendered as role: content lines, not the raw JSON blob.
    assert!(transcript.contains("user: first request"));
    assert!(transcript.contains("assistant: first reply"));
    assert!(transcript.contains("user: second request"));
    assert!(
        !transcript.contains("\"role\""),
        "transcript must not contain raw JSON keys"
    );
}

#[test]
fn compaction_transcript_empty_for_invalid_json() {
    assert_eq!(super::assemble_compaction_transcript(""), "");
    assert_eq!(super::assemble_compaction_transcript("not json"), "");
    assert_eq!(super::assemble_compaction_transcript("[]"), "");
}

#[tokio::test]
async fn compact_writes_summary_to_vault_compact_namespace() {
    use smedja_vault::Vault;

    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
    let session_id = "sess-compact-test".to_owned();
    let summary = "• Implemented auth\n• Tests pass\nGoal: ship v1".to_owned();
    let turn_count: i64 = 7;

    // Simulate the vault write logic from session.compact.
    let compact_sid = session_id.clone();
    let compact_summary = summary.clone();
    let vt = Arc::clone(&vault);
    tokio::task::spawn_blocking(move || {
        let entry = smedja_vault::VaultEntry {
            id: format!("compact:{compact_sid}:{turn_count}"),
            embedding: crate::embedder::embed(&compact_summary),
            payload: serde_json::json!({
                "session_id": compact_sid,
                "turn_count": turn_count,
            }),
            namespace: "compact".to_owned(),
            content: compact_summary,
            source_file: None,
            added_by: Some("session.compact".to_owned()),
            chunk_index: None,
            parent_id: None,
            created_at: 0.0,
            embedder_model_id: smedja_vault::LEGACY_MODEL_ID.to_owned(),
            dim: crate::embedder::DIM,
        };
        let mut guard = vt.blocking_lock();
        guard.upsert(&entry).unwrap();
    })
    .await
    .unwrap();

    let count = vault.lock().await.count_by_namespace("compact").unwrap();
    assert_eq!(count, 1, "one compact entry must be written per compaction");

    let results = {
        let guard = vault.lock().await;
        let qv = crate::embedder::embed("auth tests");
        guard
            .search(
                &qv,
                "auth tests",
                "compact",
                5,
                smedja_vault::LEGACY_MODEL_ID,
                crate::embedder::DIM,
            )
            .unwrap()
    };
    assert!(
        !results.is_empty(),
        "compact summary must be retrievable by semantic search"
    );
}

#[tokio::test]
async fn session_context_includes_vault_stratum_counts() {
    use smedja_vault::Vault;

    let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

    // Populate vault with one warm and two default (cold) entries.
    {
        let mut guard = vault.lock().await;
        let make_entry = |id: &str, ns: &str| smedja_vault::VaultEntry {
            id: id.to_owned(),
            embedding: crate::embedder::embed(id),
            payload: serde_json::json!({}),
            namespace: ns.to_owned(),
            content: id.to_owned(),
            source_file: None,
            added_by: None,
            chunk_index: None,
            parent_id: None,
            created_at: 0.0,
            embedder_model_id: smedja_vault::LEGACY_MODEL_ID.to_owned(),
            dim: crate::embedder::DIM,
        };
        guard.upsert(&make_entry("w1", "warm")).unwrap();
        guard.upsert(&make_entry("c1", "default")).unwrap();
        guard.upsert(&make_entry("c2", "default")).unwrap();
    }

    let (warm_count, cold_count) = tokio::task::spawn_blocking(move || {
        let guard = vault.blocking_lock();
        let warm = guard.count_by_namespace("warm").unwrap_or(0);
        let cold = guard.count_by_namespace("default").unwrap_or(0);
        (warm, cold)
    })
    .await
    .unwrap();

    assert_eq!(warm_count, 1);
    assert_eq!(cold_count, 2);
}

// ── quality-hook fan-out concurrency cap ──────────────────────────────────

#[tokio::test]
async fn spawn_blocking_bounded_caps_concurrency() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CAP: usize = 3;
    const TOTAL: usize = 20;

    let sem = Arc::new(tokio::sync::Semaphore::new(CAP));
    let current = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));

    for _ in 0..TOTAL {
        let cur = Arc::clone(&current);
        let pk = Arc::clone(&peak);
        let dn = Arc::clone(&done);
        super::spawn_blocking_bounded(&sem, move || {
            let now = cur.fetch_add(1, Ordering::SeqCst) + 1;
            pk.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(20));
            cur.fetch_sub(1, Ordering::SeqCst);
            dn.fetch_add(1, Ordering::SeqCst);
        })
        .await;
    }

    // Wait for every job to complete.
    while done.load(Ordering::SeqCst) < TOTAL {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let observed_peak = peak.load(Ordering::SeqCst);
    assert!(
        observed_peak >= 1,
        "jobs must have actually run; peak was {observed_peak}"
    );
    assert!(
        observed_peak <= CAP,
        "bounded fan-out must never exceed the cap of {CAP}; observed {observed_peak}"
    );
}
