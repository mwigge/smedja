//! Session handler unit tests, moved verbatim from `session.rs`.
//! `super::*` resolves to the session module and its re-exports.

use super::*;
use smedja_ingot::{Ingot, IngotHandle};

#[test]
fn canonical_runner_parser_tolerates_cli_suffix_and_rejects_unknown() {
    use smedja_assayer::Runner;

    use crate::common::parse_runner_str;
    assert_eq!(parse_runner_str("claude"), Some(Runner::Claude));
    assert_eq!(parse_runner_str("claude-cli"), Some(Runner::Claude));
    assert_eq!(parse_runner_str("anthropic"), Some(Runner::Claude));
    assert_eq!(parse_runner_str("codex-cli"), Some(Runner::Codex));
    assert_eq!(parse_runner_str("openai"), Some(Runner::Codex));
    assert_eq!(parse_runner_str("kimi"), Some(Runner::Kimi));
    assert_eq!(parse_runner_str("kimi-cli"), Some(Runner::Kimi));
    assert_eq!(parse_runner_str("moonshot"), Some(Runner::Kimi));
    assert_eq!(parse_runner_str("gemini"), Some(Runner::Gemini));
    assert_eq!(parse_runner_str("gemini-cli"), Some(Runner::Gemini));
    assert_eq!(parse_runner_str("google"), Some(Runner::Gemini));
    assert_eq!(parse_runner_str("LOCAL"), Some(Runner::Local));
    assert_eq!(parse_runner_str("minimax"), Some(Runner::Minimax));
    assert_eq!(parse_runner_str("pool"), Some(Runner::Pool));
    assert_eq!(parse_runner_str("opencode"), Some(Runner::OpenCode));
    assert_eq!(parse_runner_str("nope"), None);
}

#[test]
fn parse_tier_name_maps_known_tiers() {
    use smedja_assayer::Tier;
    assert_eq!(parse_tier_name("fast"), Some(Tier::Fast));
    assert_eq!(parse_tier_name("deep"), Some(Tier::Deep));
    assert_eq!(parse_tier_name("local"), Some(Tier::Local));
    assert_eq!(parse_tier_name("ultra"), None);
}

// ── session.set_effort ─────────────────────────────────────────────────────

#[test]
fn parse_effort_name_maps_known_levels() {
    use smedja_types::Effort;
    assert_eq!(parse_effort_name("low"), Some(Effort::Low));
    assert_eq!(parse_effort_name("medium"), Some(Effort::Medium));
    assert_eq!(parse_effort_name("high"), Some(Effort::High));
    assert_eq!(parse_effort_name(" HIGH "), Some(Effort::High));
    assert_eq!(parse_effort_name("max"), None);
}

#[test]
fn set_effort_pins_and_default_clears() {
    let id = Uuid::new_v4().to_string();
    assert_eq!(session_effort(&id), None, "no pin before set_effort");

    let resp = set_effort_with(&id, "high").unwrap();
    assert_eq!(resp["effort"].as_str().unwrap(), "high");
    assert_eq!(session_effort(&id), Some(smedja_types::Effort::High));

    let resp = set_effort_with(&id, "default").unwrap();
    assert!(resp["effort"].is_null(), "default clears the pin");
    assert_eq!(session_effort(&id), None);
}

#[test]
fn set_effort_rejects_unknown_level() {
    let id = Uuid::new_v4().to_string();
    let err = set_effort_with(&id, "ultra").unwrap_err();
    assert_eq!(err.code, smedja_rpc::codes::INVALID_PARAMS);
    assert_eq!(session_effort(&id), None, "a rejected level must not pin");
}

#[tokio::test]
async fn set_effort_rejects_unknown_session() {
    let ig = handle();
    let err = ensure_session_exists(&ig, "no-such-session")
        .await
        .unwrap_err();
    assert_eq!(err.code, smedja_rpc::codes::INVALID_PARAMS);
    assert!(err.message.contains("unknown session"), "got: {err:?}");
}

#[test]
fn effort_override_cap_rejects_new_sessions_but_allows_repins() {
    let mut map = std::collections::HashMap::new();
    for i in 0..1024 {
        insert_capped(&mut map, &format!("sess-{i}"), smedja_types::Effort::Low).unwrap();
    }
    let err = insert_capped(&mut map, "one-too-many", smedja_types::Effort::Low).unwrap_err();
    assert!(
        err.message.contains("too many effort overrides"),
        "got: {err:?}"
    );
    // Re-pinning an existing session is not a new entry and stays allowed.
    insert_capped(&mut map, "sess-0", smedja_types::Effort::High).unwrap();
    assert_eq!(map["sess-0"], smedja_types::Effort::High);
}

#[test]
fn prune_effort_overrides_drops_unknown_sessions() {
    let kept = Uuid::new_v4().to_string();
    let dropped = Uuid::new_v4().to_string();
    set_effort_with(&kept, "low").unwrap();
    set_effort_with(&dropped, "low").unwrap();

    let valid = std::collections::HashSet::from([kept.clone()]);
    prune_effort_overrides(&valid);

    assert_eq!(session_effort(&kept), Some(smedja_types::Effort::Low));
    assert_eq!(
        session_effort(&dropped),
        None,
        "pruned session must lose its pin"
    );
    clear_effort_override(&kept);
}

fn handle() -> IngotHandle {
    IngotHandle::new(Ingot::open_in_memory().unwrap())
}

// ── session.set_model validation ───────────────────────────────────────────

struct NullProvider;
impl smedja_adapter::Provider for NullProvider {
    fn stream_chat(
        &self,
        _messages: &[smedja_adapter::Message],
        _opts: &smedja_adapter::CallOptions,
    ) -> smedja_adapter::DeltaStream {
        Box::pin(futures_util::stream::empty())
    }
}

fn pool_with(
    entries: Vec<(
        (smedja_assayer::Runner, smedja_assayer::Tier),
        &'static str,
        &'static str,
    )>,
) -> crate::provider_pool::ProviderPool {
    use smedja_assayer::{Runner, Tier};
    let mut map = std::collections::HashMap::new();
    let mut order = Vec::new();
    let mut default: Option<(Runner, Tier)> = None;
    for (key, runner_name, default_model) in entries {
        if default.is_none() {
            default = Some(key);
        }
        if map
            .insert(
                key,
                crate::provider_pool::ProviderEntry {
                    provider: Box::new(NullProvider),
                    runner: key.0,
                    tier: key.1,
                    runner_name,
                    default_model: default_model.to_owned(),
                },
            )
            .is_none()
        {
            order.push(key);
        }
    }
    crate::provider_pool::ProviderPool {
        entries: map,
        order,
        default,
        local: None,
    }
}

#[test]
fn set_model_advisory_for_api_and_cli_runners() {
    use smedja_assayer::{Runner, Tier};
    // CLI/API runners: an unknown model id is advisory-accepted — providers
    // ship new models faster than the pool config tracks them.
    let cli_pool = pool_with(vec![(
        (Runner::Kimi, Tier::Fast),
        "kimi-cli",
        "kimi-code/kimi-for-coding-highspeed",
    )]);
    let known = known_models_for_runner(&cli_pool, "kimi-cli");
    assert!(!known.is_empty());
    assert_eq!(set_model_rejection("kimi-cli", "kimi-k99", &known), None);

    let api_pool = pool_with(vec![((Runner::Kimi, Tier::Fast), "moonshot", "kimi-k3")]);
    let known = known_models_for_runner(&api_pool, "moonshot");
    assert_eq!(set_model_rejection("moonshot", "kimi-k99", &known), None);

    // The local inventory IS authoritative: unknown local models are rejected.
    let local_known = vec!["qwen3-14b".to_owned()];
    assert!(set_model_rejection("local", "fresh-model", &local_known).is_some());
    assert_eq!(
        set_model_rejection("local", "qwen3-14b", &local_known),
        None
    );
    // A local endpoint with no known inventory accepts (nothing to check against).
    assert_eq!(set_model_rejection("local", "anything", &[]), None);
}

#[test]
fn set_model_local_inventory_updates_after_install() {
    use crate::provider_pool::LocalControl;
    use smedja_adapter::{GpuSnapshot, LocalModel};
    let control = LocalControl::new(
        "http://127.0.0.1:9090".to_owned(),
        "http://127.0.0.1:9090".to_owned(),
        vec![LocalModel {
            id: "qwen3-14b".to_owned(),
            est_vram_mb: Some(9000),
        }],
        GpuSnapshot::none(),
        Some("qwen3-14b".to_owned()),
    );
    let mut pool = pool_with(vec![]);
    pool.local = Some(control);

    let known = known_models_for_runner(&pool, "local");
    assert_eq!(known, vec!["qwen3-14b".to_owned()]);
    assert!(
        !known.contains(&"fresh-model".to_owned()),
        "an uninstalled local model is rejected by set_model"
    );

    // After local.install confirms the model is servable it joins the
    // inventory, so the same pin is now valid.
    pool.local_control()
        .expect("local control")
        .add_inventory_model("fresh-model");
    let known = known_models_for_runner(&pool, "local");
    assert!(known.contains(&"fresh-model".to_owned()));
}

#[test]
fn known_models_for_runner_covers_pool_unknown_and_absent_runners() {
    use smedja_assayer::{Runner, Tier};
    let pool = pool_with(vec![
        ((Runner::Codex, Tier::Fast), "codex-cli", "gpt-5.5"),
        ((Runner::Codex, Tier::Deep), "codex-cli", "gpt-5.5-pro"),
    ]);
    // A pooled non-local runner yields its per-tier defaults in probe order.
    assert_eq!(
        known_models_for_runner(&pool, "codex"),
        vec!["gpt-5.5".to_owned(), "gpt-5.5-pro".to_owned()]
    );
    // An unparseable runner string has no known models (set_model then
    // accepts any value rather than rejecting against an empty list).
    assert!(known_models_for_runner(&pool, "nope").is_empty());
    // A parseable runner absent from the pool likewise has no known models.
    assert!(known_models_for_runner(&pool, "gemini").is_empty());
    // `local` without local control (no endpoint detected) has no inventory.
    assert!(known_models_for_runner(&pool, "local").is_empty());
}

fn sample_session(id: Uuid, title: &str) -> Session {
    let now = Timestamp::now();
    Session {
        id,
        created_at: now,
        updated_at: now,
        status: "active".to_owned(),
        task_id: None,
        mode: None,
        title: title.to_owned(),
        cowork_mode: false,
        workspace_root: None,
        model_override: None,
        runner_override: None,
    }
}

// ── session.search ────────────────────────────────────────────────────────

#[tokio::test]
async fn search_matches_title_substring() {
    let ig = handle();
    let id = Uuid::new_v4();
    ig.create_session(sample_session(id, "rust memory pressure"))
        .await
        .unwrap();
    let resp = search_with(&ig, "memory").await.unwrap();
    let arr = resp.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"].as_str().unwrap(), id.to_string());
}

#[tokio::test]
async fn search_returns_empty_for_no_match() {
    let ig = handle();
    ig.create_session(sample_session(Uuid::new_v4(), "alpha"))
        .await
        .unwrap();
    let resp = search_with(&ig, "zzznomatch").await.unwrap();
    assert_eq!(resp.as_array().unwrap().len(), 0);
}

// ── session.list ──────────────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_empty_when_no_sessions() {
    let ig = handle();
    let resp = list_with(&ig).await.unwrap();
    assert_eq!(resp, Value::Array(vec![]));
}

#[tokio::test]
async fn list_returns_all_created_sessions() {
    let ig = handle();
    let id_a = Uuid::new_v4();
    let id_b = Uuid::new_v4();
    ig.create_session(sample_session(id_a, "alpha"))
        .await
        .unwrap();
    ig.create_session(sample_session(id_b, "beta"))
        .await
        .unwrap();

    let resp = list_with(&ig).await.unwrap();
    let arr = resp.as_array().unwrap();
    assert_eq!(arr.len(), 2, "expected two sessions");
    let titles: Vec<&str> = arr.iter().map(|v| v["title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"alpha"), "missing 'alpha'");
    assert!(titles.contains(&"beta"), "missing 'beta'");
}

#[tokio::test]
async fn list_caps_at_ten_most_recent_sessions() {
    let ig = handle();
    for i in 0u8..15 {
        ig.create_session(sample_session(Uuid::new_v4(), &format!("s{i}")))
            .await
            .unwrap();
    }
    let resp = list_with(&ig).await.unwrap();
    let arr = resp.as_array().unwrap();
    assert_eq!(arr.len(), 10, "must return at most 10 sessions");
    // The last 10 created are s5..s14; the first 5 (s0..s4) are dropped.
    let titles: Vec<&str> = arr.iter().map(|v| v["title"].as_str().unwrap()).collect();
    assert!(
        titles.contains(&"s14"),
        "most recent session must be present"
    );
    assert!(!titles.contains(&"s4"), "oldest sessions must be dropped");
}

// ── session.fork ─────────────────────────────────────────────────────────

#[tokio::test]
async fn fork_creates_new_session_with_same_title() {
    let ig = handle();
    let parent_id = Uuid::new_v4();
    ig.create_session(sample_session(parent_id, "my-session"))
        .await
        .unwrap();

    let resp = fork_with(&ig, parent_id.to_string(), None).await.unwrap();

    // The response reports the new session id and the parent.
    assert_eq!(resp["forked_from"], parent_id.to_string());
    let new_id = resp["session_id"].as_str().unwrap();
    assert_ne!(new_id, parent_id.to_string(), "forked id must differ");

    // The new session must exist in the store with the same title.
    let new_sess = ig.get_session(new_id).await.unwrap().unwrap();
    assert_eq!(new_sess.title, "my-session");
    assert_eq!(new_sess.status, "active");
}

#[tokio::test]
async fn fork_has_checkpoint_false_when_no_checkpoint() {
    let ig = handle();
    let parent_id = Uuid::new_v4();
    ig.create_session(sample_session(parent_id, "s"))
        .await
        .unwrap();

    let resp = fork_with(&ig, parent_id.to_string(), None).await.unwrap();
    assert_eq!(resp["has_checkpoint"], false);
}

#[tokio::test]
async fn fork_copies_checkpoint_into_forked_session() {
    let ig = handle();
    let parent_id = Uuid::new_v4();
    ig.create_session(sample_session(parent_id, "s"))
        .await
        .unwrap();
    ig.save_checkpoint(Checkpoint {
        id: Uuid::new_v4(),
        session_id: parent_id.to_string(),
        turn_n: 3,
        messages_json: r#"["hello"]"#.to_owned(),
        created_at: Timestamp::now(),
        compaction_id: None,
    })
    .await
    .unwrap();

    let resp = fork_with(&ig, parent_id.to_string(), None).await.unwrap();
    assert_eq!(resp["has_checkpoint"], true, "checkpoint should be copied");

    let new_id = resp["session_id"].as_str().unwrap();
    let cp = ig.latest_checkpoint(new_id).await.unwrap();
    assert!(cp.is_some(), "forked session must have a checkpoint");
    assert_eq!(cp.unwrap().turn_n, 3);
}

#[tokio::test]
async fn fork_returns_error_for_unknown_session() {
    let ig = handle();
    let err = fork_with(&ig, "no-such-id".to_owned(), None)
        .await
        .unwrap_err();
    assert_eq!(err.code, smedja_rpc::codes::INTERNAL_ERROR);
}

// --- WI-018 GAP C: session.fork at arbitrary turn_n ----------------------

#[tokio::test]
async fn fork_at_turn_n_selects_closest_checkpoint() {
    let ig = handle();
    let parent_id = Uuid::new_v4();
    ig.create_session(sample_session(parent_id, "s"))
        .await
        .unwrap();
    for turn in [1i64, 3, 5] {
        ig.save_checkpoint(Checkpoint {
            id: Uuid::new_v4(),
            session_id: parent_id.to_string(),
            turn_n: turn,
            messages_json: format!(r#"["turn-{turn}"]"#),
            created_at: Timestamp::now(),
            compaction_id: None,
        })
        .await
        .unwrap();
    }

    // Fork at turn 4 → closest checkpoint not exceeding 4 is turn 3.
    let resp = fork_with(&ig, parent_id.to_string(), Some(4))
        .await
        .unwrap();
    assert_eq!(resp["has_checkpoint"], true);
    let new_id = resp["session_id"].as_str().unwrap();
    let cp = ig.latest_checkpoint(new_id).await.unwrap().unwrap();
    assert_eq!(
        cp.turn_n, 3,
        "expected checkpoint at turn 3, got {}",
        cp.turn_n
    );
}

#[tokio::test]
async fn fork_at_turn_n_past_last_returns_error() {
    let ig = handle();
    let parent_id = Uuid::new_v4();
    ig.create_session(sample_session(parent_id, "s"))
        .await
        .unwrap();
    // No checkpoints exist.
    let err = fork_with(&ig, parent_id.to_string(), Some(99))
        .await
        .unwrap_err();
    assert_eq!(
        err.code,
        smedja_rpc::codes::INTERNAL_ERROR,
        "must error when no checkpoints"
    );
}

#[tokio::test]
async fn fork_at_turn_n_before_all_checkpoints_returns_error() {
    let ig = handle();
    let parent_id = Uuid::new_v4();
    ig.create_session(sample_session(parent_id, "s"))
        .await
        .unwrap();
    ig.save_checkpoint(Checkpoint {
        id: Uuid::new_v4(),
        session_id: parent_id.to_string(),
        turn_n: 5,
        messages_json: r#"["hello"]"#.to_owned(),
        created_at: Timestamp::now(),
        compaction_id: None,
    })
    .await
    .unwrap();
    // Request turn 2 but the only checkpoint is at turn 5.
    let err = fork_with(&ig, parent_id.to_string(), Some(2))
        .await
        .unwrap_err();
    assert_eq!(
        err.code,
        smedja_rpc::codes::INTERNAL_ERROR,
        "must error when no checkpoint <= requested turn"
    );
}

// ── session.set_runner ────────────────────────────────────────────────────

#[tokio::test]
async fn set_runner_clears_stale_model_override() {
    let ig = handle();
    let id = Uuid::new_v4();
    ig.create_session(sample_session(id, "s")).await.unwrap();
    // Pin a codex model (as /tier or /model would on a codex session).
    ig.update_session_model_override(&id.to_string(), "gpt-5.5")
        .await
        .unwrap();

    let resp = set_runner_with(&ig, &id.to_string(), "kimi").await.unwrap();
    assert_eq!(resp["runner"].as_str().unwrap(), "kimi-cli");
    assert_eq!(
        resp["model_pin_cleared"].as_bool(),
        Some(true),
        "a pinned model must report as cleared"
    );

    let sess = ig.get_session(&id.to_string()).await.unwrap().unwrap();
    assert_eq!(sess.runner_override.as_deref(), Some("kimi-cli"));
    assert_eq!(
        sess.model_override, None,
        "a model pinned for the old runner must not leak onto the new one"
    );
}

#[tokio::test]
async fn set_runner_without_pin_reports_not_cleared() {
    let ig = handle();
    let id = Uuid::new_v4();
    ig.create_session(sample_session(id, "s")).await.unwrap();

    let resp = set_runner_with(&ig, &id.to_string(), "kimi").await.unwrap();
    assert_eq!(
        resp["model_pin_cleared"].as_bool(),
        Some(false),
        "no pin existed — the client must not claim one was cleared"
    );
}

#[tokio::test]
async fn set_runner_rejects_unknown_runner() {
    let ig = handle();
    let id = Uuid::new_v4();
    ig.create_session(sample_session(id, "s")).await.unwrap();
    let err = set_runner_with(&ig, &id.to_string(), "nope")
        .await
        .unwrap_err();
    assert_eq!(err.code, smedja_rpc::codes::INVALID_PARAMS);
}

// ── session.takeover model inheritance ────────────────────────────────────

#[test]
fn takeover_drops_model_pin_when_switching_runner_family() {
    let mut parent = sample_session(Uuid::new_v4(), "s");
    parent.runner_override = Some("codex-cli".into());
    parent.model_override = Some("gpt-5.5".into());
    assert_eq!(
        inherited_takeover_model(&parent, "kimi-cli", "claude-cli"),
        None,
        "a codex model pin must not leak onto a kimi takeover"
    );
}

#[test]
fn takeover_keeps_model_pin_when_staying_on_runner() {
    let mut parent = sample_session(Uuid::new_v4(), "s");
    parent.runner_override = Some("codex-cli".into());
    parent.model_override = Some("gpt-5.5".into());
    assert_eq!(
        inherited_takeover_model(&parent, "codex-cli", "claude-cli"),
        Some("gpt-5.5".to_owned()),
    );
}

#[test]
fn takeover_compares_against_startup_runner_when_parent_has_no_override() {
    let mut parent = sample_session(Uuid::new_v4(), "s");
    parent.model_override = Some("claude-opus-4-8".into());
    // No runner_override: the pin belongs to the startup runner's family.
    assert_eq!(
        inherited_takeover_model(&parent, "claude-cli", "claude-cli"),
        Some("claude-opus-4-8".to_owned()),
    );
    assert_eq!(
        inherited_takeover_model(&parent, "kimi-cli", "claude-cli"),
        None,
    );
}
