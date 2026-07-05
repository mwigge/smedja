//! Session handler unit tests, moved verbatim from `session.rs`.
//! `super::*` resolves to the session module and its re-exports.

use super::*;
use smedja_ingot::{Ingot, IngotHandle};

#[test]
fn parse_runner_name_tolerates_cli_suffix_and_rejects_unknown() {
    use smedja_assayer::Runner;
    assert_eq!(parse_runner_name("claude"), Some(Runner::Claude));
    assert_eq!(parse_runner_name("claude-cli"), Some(Runner::Claude));
    assert_eq!(parse_runner_name("codex-cli"), Some(Runner::Codex));
    assert_eq!(parse_runner_name("LOCAL"), Some(Runner::Local));
    assert_eq!(parse_runner_name("minimax"), Some(Runner::Minimax));
    assert_eq!(parse_runner_name("nope"), None);
}

#[test]
fn parse_tier_name_maps_known_tiers() {
    use smedja_assayer::Tier;
    assert_eq!(parse_tier_name("fast"), Some(Tier::Fast));
    assert_eq!(parse_tier_name("deep"), Some(Tier::Deep));
    assert_eq!(parse_tier_name("local"), Some(Tier::Local));
    assert_eq!(parse_tier_name("ultra"), None);
}

fn handle() -> IngotHandle {
    IngotHandle::new(Ingot::open_in_memory().unwrap())
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
