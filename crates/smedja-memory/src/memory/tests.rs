use std::sync::Arc;

use super::working::floor_char_boundary;
use super::*;
use crate::types::{Message, Role, Stratum};

fn make_mem(n: usize) -> WorkingMemory {
    let mut m = WorkingMemory::new(4096);
    for i in 0..n {
        m.push(Message::user(format!("msg {i}")));
    }
    m
}

#[test]
fn new_memory_is_empty() {
    let m = WorkingMemory::new(4096);
    assert!(m.is_empty());
    assert_eq!(m.len(), 0);
}

#[test]
fn push_adds_message() {
    let mut m = WorkingMemory::new(4096);
    m.push(Message::user("hello"));
    assert!(!m.is_empty());
}

#[test]
fn len_after_push() {
    let mut m = WorkingMemory::new(4096);
    m.push(Message::user("a"));
    m.push(Message::user("b"));
    assert_eq!(m.len(), 2);
}

#[test]
fn seal_prefix_freezes_count() {
    let mut m = WorkingMemory::new(4096);
    m.push(Message::system("sys"));
    m.push(Message::user("skills"));
    m.seal_prefix();
    assert_eq!(m.stable_prefix(), 2);
    m.push(Message::user("hello"));
    // prefix boundary must not change after more pushes
    assert_eq!(m.stable_prefix(), 2);
    assert_eq!(m.len(), 3);
}

#[test]
fn mutable_window_excludes_prefix() {
    let mut m = WorkingMemory::new(4096);
    m.push(Message::system("sys"));
    m.seal_prefix();
    m.push(Message::user("turn1"));
    m.push(Message::user("turn2"));
    let win = m.mutable_window();
    assert_eq!(win.len(), 2);
    assert_eq!(win[0].content, "turn1");
}

#[test]
fn replace_mutable_keeps_prefix() {
    let mut m = WorkingMemory::new(4096);
    m.push(Message::system("sys"));
    m.seal_prefix();
    m.push(Message::user("old1"));
    m.push(Message::user("old2"));

    m.replace_mutable(vec![Message::assistant("summary")]);

    assert_eq!(m.len(), 2); // 1 prefix + 1 replacement
    assert_eq!(m.messages()[0].content, "sys");
    assert_eq!(m.messages()[1].content, "summary");
}

#[test]
fn messages_returns_all_in_order() {
    let m = make_mem(3);
    let msgs = m.messages();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].content, "msg 0");
    assert_eq!(msgs[2].content, "msg 2");
}

#[test]
fn stratum_for_recent_is_hot() {
    let m = make_mem(10);
    // last index (9) should be Hot
    assert_eq!(m.stratum_for(9), Stratum::Hot);
    // index 5 = 10-1-5 = 4 from end → within HOT_WINDOW(5) → Hot
    assert_eq!(m.stratum_for(5), Stratum::Hot);
}

#[test]
fn stratum_for_older_is_warm() {
    // 20 messages; index 9 → 20-1-9 = 10 from end → beyond HOT(5), within WARM(30)
    let m = make_mem(20);
    assert_eq!(m.stratum_for(9), Stratum::Warm);
}

#[test]
fn stratum_for_oldest_is_cold() {
    // 50 messages; index 0 → 50-1-0 = 49 from end → beyond WARM(30) → Cold
    let m = make_mem(50);
    assert_eq!(m.stratum_for(0), Stratum::Cold);
}

#[test]
fn load_skills_empty_when_dir_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let result = super::load_workspace_skills(tmp.path()).unwrap();
    assert!(result.is_empty());
}

#[test]
fn load_role_skills_reads_file_and_dir_for_the_role_only() {
    let tmp = tempfile::tempdir().unwrap();
    let roles = tmp.path().join(".smedja").join("roles");
    std::fs::create_dir_all(roles.join("review")).unwrap();
    std::fs::write(roles.join("review.md"), b"top review rule").unwrap();
    std::fs::write(roles.join("review").join("a_extra.md"), b"extra A").unwrap();
    std::fs::write(roles.join("plan.md"), b"a plan rule").unwrap();

    let review = super::load_role_skills(tmp.path(), "review").unwrap();
    assert_eq!(
        review,
        vec!["top review rule".to_owned(), "extra A".to_owned()]
    );

    // A role with no pack yields nothing; an unrelated role isn't mixed in.
    assert!(super::load_role_skills(tmp.path(), "research")
        .unwrap()
        .is_empty());
    assert_eq!(
        super::load_role_skills(tmp.path(), "plan").unwrap(),
        vec!["a plan rule".to_owned()]
    );
}

#[test]
fn load_role_skills_rejects_traversal_roles() {
    // A sibling file outside the roles dir that a `../` role could reach.
    let tmp = tempfile::tempdir().unwrap();
    let roles = tmp.path().join(".smedja").join("roles");
    std::fs::create_dir_all(&roles).unwrap();
    // Plant a readable file one level above the roles dir.
    std::fs::write(tmp.path().join(".smedja").join("secret.md"), b"secret").unwrap();

    for role in ["../etc", "a/b", "/abs", "..", ".", ""] {
        let result = super::load_role_skills(tmp.path(), role).unwrap();
        assert!(
            result.is_empty(),
            "traversal role {role:?} must yield an empty result"
        );
    }
}

#[test]
fn load_role_skills_accepts_normal_role() {
    let tmp = tempfile::tempdir().unwrap();
    let roles = tmp.path().join(".smedja").join("roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join("myskill.md"), b"role rule").unwrap();

    let result = super::load_role_skills(tmp.path(), "myskill").unwrap();
    assert_eq!(result, vec!["role rule".to_owned()]);
}

#[test]
fn load_skills_reads_md_files() {
    let tmp = tempfile::tempdir().unwrap();
    let skills_dir = tmp.path().join(".smedja").join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(skills_dir.join("alpha.md"), "skill alpha").unwrap();
    std::fs::write(skills_dir.join("beta.md"), "skill beta").unwrap();
    let mut result = super::load_workspace_skills(tmp.path()).unwrap();
    result.sort();
    assert_eq!(result, vec!["skill alpha", "skill beta"]);
}

#[test]
fn load_skills_ignores_non_md() {
    let tmp = tempfile::tempdir().unwrap();
    let skills_dir = tmp.path().join(".smedja").join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(skills_dir.join("skill.md"), "md content").unwrap();
    std::fs::write(skills_dir.join("readme.txt"), "txt content").unwrap();
    let result = super::load_workspace_skills(tmp.path()).unwrap();
    assert_eq!(result, vec!["md content"]);
}

#[test]
fn strata_config_fast_has_shallow_warm() {
    let cfg = StrataConfig::fast();
    assert_eq!(cfg.hot_depth, 5);
    assert_eq!(cfg.warm_depth, 10);
}

#[test]
fn strata_config_from_tier_local() {
    let cfg = StrataConfig::from_tier("local");
    assert_eq!(cfg.warm_depth, 15);
}

#[test]
fn set_strata_changes_stratum_for_result() {
    // With fast config (warm_depth=10), turn at index 6 from end=4 is Hot.
    // With deep config (warm_depth=30), same turn is Warm when there are >10 messages.
    let mut m = WorkingMemory::new(4096);
    for _ in 0..20 {
        m.push(Message::user("x"));
    }
    m.set_strata(StrataConfig::fast());
    // index 9 = from_end=10 → beyond hot(5), beyond warm(10) → Cold under fast
    assert_eq!(m.stratum_for(9), Stratum::Cold);

    m.set_strata(StrataConfig::deep());
    // same index → from_end=10 → within warm(30) → Warm under deep
    assert_eq!(m.stratum_for(9), Stratum::Warm);
}

#[test]
fn detect_agents_md_absent_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let result = detect_agents_md(tmp.path()).unwrap();
    assert!(result.is_none());
}

#[test]
fn build_prompt_empty_returns_empty() {
    let m = WorkingMemory::new(4096);
    assert!(m.build_prompt(4096).is_empty());
}

#[test]
fn build_prompt_includes_hot_turns() {
    let mut m = WorkingMemory::new(4096);
    m.push(Message::system("sys"));
    m.seal_prefix();
    for i in 0..10 {
        m.push(Message::user(format!("turn {i}")));
    }
    // With default deep config (hot_depth=5), last 5 turns always included.
    let prompt = m.build_prompt(4096);
    // Prefix (1) + at least 5 hot turns = at least 6 messages.
    assert!(
        prompt.len() >= 6,
        "expected at least 6 messages, got {}",
        prompt.len()
    );
}

#[test]
fn build_prompt_respects_budget_for_warm() {
    let mut m = WorkingMemory::new(4096);
    m.push(Message::system("sys"));
    m.seal_prefix();
    // Push many long warm-zone messages.
    for i in 0..40 {
        m.push(Message::user(format!(
            "warm message {i:03} with some extra content to cost tokens"
        )));
    }
    // Very tight budget: only fit prefix + hot turns.
    let budget = 10; // tiny budget
    let prompt_tight = m.build_prompt(budget);
    let prompt_full = m.build_prompt(100_000);
    // With a tight budget, we get fewer messages than with a full budget.
    assert!(prompt_tight.len() <= prompt_full.len());
}

#[test]
fn build_prompt_with_omitted_reports_dropped_cold_tokens() {
    let mut m = WorkingMemory::new(4096);
    m.push(Message::system("sys"));
    m.seal_prefix();
    // Many turns push older ones into the cold stratum (deep: hot=5, warm=30).
    for i in 0..50 {
        m.push(Message::user(format!(
            "turn {i:03} with some content to estimate tokens for omission"
        )));
    }
    let (prompt, omitted) = m.build_prompt_with_omitted(100_000);
    // Cold turns beyond the warm window are dropped → a positive estimate.
    assert!(
        omitted > 0,
        "cold-stratum omission must report saved tokens"
    );
    // The prompt itself excludes the cold turns it counted as omitted.
    assert!(prompt.len() < m.len() + 1);
}

#[test]
fn warm_message_too_large_is_truncated_not_dropped() {
    let mut m = WorkingMemory::new(4096);
    m.push(Message::system("sys"));
    m.seal_prefix();
    // deep: hot=5, warm=30. Push large_content first, then 5 short messages.
    // After 6 mutable pushes (len=7, stable_prefix=1):
    //   large_content at abs_index=1 → from_end=5 → Warm
    //   5 short messages at abs_index=2..6 → from_end=4..0 → Hot
    let large_content = "x".repeat(400); // token_estimate = 100+1 = 101
    m.push(Message::user(large_content.clone()));
    for _ in 0..5 {
        m.push(Message::user("short"));
    }
    // Budget of 10 tokens (40 chars) < 101 → must truncate, not drop.
    let (prompt, _omitted) = m.build_prompt_with_omitted(10);
    let truncated: Vec<_> = prompt
        .iter()
        .filter(|msg| msg.content.contains("[... truncated]"))
        .collect();
    assert!(
        !truncated.is_empty(),
        "large warm message must be truncated and included, not dropped"
    );
    assert!(truncated[0].content.len() < large_content.len());
}

#[test]
fn warm_message_fits_exactly_is_not_truncated() {
    let mut m = WorkingMemory::new(4096);
    m.push(Message::system("sys"));
    m.seal_prefix();
    // "12345678" = 8 chars → token_estimate = 8/4+1 = 3. Budget = 3 → exact fit.
    // Push it first, then 5 short messages so it lands in the Warm stratum.
    m.push(Message::user("12345678"));
    for _ in 0..5 {
        m.push(Message::user("short"));
    }
    let (prompt, _) = m.build_prompt_with_omitted(3);
    let truncated: Vec<_> = prompt
        .iter()
        .filter(|msg| msg.content.contains("[... truncated]"))
        .collect();
    assert!(
        truncated.is_empty(),
        "message that fits exactly must not be marked as truncated"
    );
}

#[test]
fn build_prompt_with_omitted_zero_when_all_fit() {
    let mut m = WorkingMemory::new(4096);
    m.push(Message::system("sys"));
    m.seal_prefix();
    for i in 0..3 {
        m.push(Message::user(format!("turn {i}")));
    }
    // All turns are hot and fit; nothing is omitted.
    let (_, omitted) = m.build_prompt_with_omitted(100_000);
    assert_eq!(omitted, 0);
}

#[test]
fn inject_workspace_skills_pushes_system_message() {
    let tmp = tempfile::tempdir().unwrap();
    let skills_dir = tmp.path().join(".smedja").join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(skills_dir.join("skill.md"), "do something").unwrap();
    let mut mem = WorkingMemory::new(4096);
    let n = inject_workspace_skills(&mut mem, tmp.path()).unwrap();
    assert_eq!(n, 1);
    assert_eq!(mem.len(), 1);
    assert!(mem.messages()[0].content.contains("workspace skills"));
}

#[test]
fn inject_workspace_skills_empty_dir_no_push() {
    let tmp = tempfile::tempdir().unwrap();
    let mut mem = WorkingMemory::new(4096);
    let n = inject_workspace_skills(&mut mem, tmp.path()).unwrap();
    assert_eq!(n, 0);
    assert!(mem.is_empty());
}

// --- smoke test equivalent (L66) ---

#[test]
fn smoke_l66_skill_injected_before_stable_prefix_watermark() {
    // Smoke L66: smj workspace skills add docs/conventions.md; start session;
    // skill content appears before stable_prefix watermark.
    let tmp = tempfile::tempdir().unwrap();
    let skills_dir = tmp.path().join(".smedja").join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(
        skills_dir.join("conventions.md"),
        "## Coding Conventions\nUse snake_case.",
    )
    .unwrap();

    let mut mem = WorkingMemory::new(4096);
    // Inject skills before sealing, as the session-start flow does.
    let n = super::inject_workspace_skills(&mut mem, tmp.path()).unwrap();
    assert_eq!(n, 1, "one skill file must be injected");
    // Seal the prefix to mark the stable boundary.
    mem.seal_prefix();
    // Push a user turn to simulate session activity.
    mem.push(Message::user("hello"));

    // The skill message must be at index 0 (before the watermark).
    let msgs = mem.messages();
    assert!(
        msgs[0].content.contains("Coding Conventions"),
        "skill content must appear in the first message (before stable_prefix)"
    );
    // stable_prefix == 1 means the skill message is the only frozen entry.
    assert_eq!(
        mem.stable_prefix(),
        1,
        "stable_prefix must be 1 (skill message sealed before user turns)"
    );
    // The mutable window must not contain the skill content.
    let mutable = mem.mutable_window();
    assert!(
        !mutable[0].content.contains("Coding Conventions"),
        "skill must not appear in the mutable window after sealing"
    );
}

/// Fake [`ColdStore`] that returns a fixed list of results and records the
/// arguments it was called with.
struct FakeColdStore {
    results: Vec<crate::cold::ColdResult>,
    last_call: std::sync::Mutex<Option<(String, String, usize)>>,
}

impl FakeColdStore {
    fn new(results: Vec<crate::cold::ColdResult>) -> Self {
        Self {
            results,
            last_call: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl crate::cold::ColdStore for FakeColdStore {
    async fn retrieve(
        &self,
        query: &str,
        namespace: &str,
        k: usize,
    ) -> Vec<crate::cold::ColdResult> {
        *self.last_call.lock().expect("lock not poisoned") =
            Some((query.to_owned(), namespace.to_owned(), k));
        self.results.clone()
    }
}

#[test]
fn with_cold_store_attaches_store_and_default_query_config() {
    let store = Arc::new(FakeColdStore::new(Vec::new()));
    let mem = WorkingMemory::new(4096).with_cold_store(store);
    // The default cold-query config targets the "compact" namespace, k = 3.
    let cfg = ColdQuery::default();
    assert_eq!(cfg.namespace, "compact");
    assert_eq!(cfg.k, 3);
    // The Debug impl reflects an attached store.
    assert!(format!("{mem:?}").contains("ColdStore"));
}

#[tokio::test]
async fn cold_context_returns_empty_without_store() {
    let m = make_mem(50);
    let ctx = m.cold_context("some query string").await;
    assert!(ctx.is_empty());
}

#[tokio::test]
async fn cold_context_returns_ranked_messages_from_store() {
    use crate::cold::ColdResult;
    let store = Arc::new(FakeColdStore::new(vec![
        ColdResult {
            content: "high relevance recall".to_owned(),
            score: 0.92,
            namespace: "compact".to_owned(),
        },
        ColdResult {
            content: "lower relevance recall".to_owned(),
            score: 0.41,
            namespace: "compact".to_owned(),
        },
    ]));
    let mem = WorkingMemory::new(4096).with_cold_store(Arc::clone(&store) as Arc<_>);

    let messages = mem.cold_context("recall query").await;

    assert_eq!(messages.len(), 2);
    // Order is preserved (descending score as supplied by the store).
    assert_eq!(messages[0].content, "high relevance recall");
    assert_eq!(messages[1].content, "lower relevance recall");
    assert!(messages.iter().all(|m| m.role == Role::System));

    // The default namespace/k were forwarded to the store.
    let call = store.last_call.lock().expect("lock not poisoned").clone();
    assert_eq!(
        call,
        Some(("recall query".to_owned(), "compact".to_owned(), 3))
    );
}

#[tokio::test]
async fn set_cold_query_overrides_namespace_and_k() {
    let store = Arc::new(FakeColdStore::new(Vec::new()));
    let mut mem = WorkingMemory::new(4096).with_cold_store(Arc::clone(&store) as Arc<_>);
    mem.set_cold_query("notes", 5);
    let _ = mem.cold_context("q").await;
    let call = store.last_call.lock().expect("lock not poisoned").clone();
    assert_eq!(call, Some(("q".to_owned(), "notes".to_owned(), 5)));
}

#[test]
fn load_context_files_empty_when_dir_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let result = super::load_context_files(tmp.path()).unwrap();
    assert!(result.is_empty());
}

#[test]
fn load_context_files_reads_md_files() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx_dir = tmp.path().join(".smedja").join("context");
    std::fs::create_dir_all(&ctx_dir).unwrap();
    std::fs::write(ctx_dir.join("a.md"), "context A").unwrap();
    std::fs::write(ctx_dir.join("b.md"), "context B").unwrap();
    let mut result = super::load_context_files(tmp.path()).unwrap();
    result.sort();
    assert_eq!(result, vec!["context A", "context B"]);
}

#[test]
fn load_context_files_ignores_non_md() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx_dir = tmp.path().join(".smedja").join("context");
    std::fs::create_dir_all(&ctx_dir).unwrap();
    std::fs::write(ctx_dir.join("notes.md"), "md").unwrap();
    std::fs::write(ctx_dir.join("raw.txt"), "txt").unwrap();
    let result = super::load_context_files(tmp.path()).unwrap();
    assert_eq!(result, vec!["md"]);
}

#[test]
fn floor_char_boundary_never_splits_a_codepoint() {
    let s = "a中🚀é"; // 1 + 3 + 4 + 2 bytes
    for i in 0..=s.len() {
        let f = floor_char_boundary(s, i);
        assert!(s.is_char_boundary(f), "floor must land on a boundary");
        assert!(f <= i, "floor must not exceed the requested max");
        // Slicing at the floored index must never panic.
        let _ = &s[..f];
    }
    assert_eq!(floor_char_boundary(s, 100), s.len());
}

#[test]
fn warm_truncation_floors_multibyte_content() {
    // Six turns: under the default strata (hot=5) the oldest turn (index 0)
    // is Warm and the rest are Hot. A tiny token budget forces that Warm
    // turn down the truncation branch, where byte_limit = budget*4 = 8 lands
    // in the middle of a 3-byte CJK codepoint. A raw `&content[..8]` panicked
    // the whole turn-assembly before the floor-to-boundary fix.
    let mut m = WorkingMemory::new(4096);
    m.push(Message::user("中".repeat(10))); // 30 bytes, becomes Warm
    for i in 0..5 {
        m.push(Message::user(format!("hot {i}")));
    }

    // Fail-before: this call panicked at the mid-codepoint slice.
    let (prompt, _omitted) = m.build_prompt_with_omitted(2);

    let truncated = prompt
        .iter()
        .find(|msg| msg.content.contains("[... truncated]"))
        .expect("the Warm turn must be present and truncated");
    let kept = truncated.content.trim_end_matches("\n[... truncated]");
    assert!(
        kept.chars().all(|c| c == '中'),
        "kept prefix must contain only whole codepoints"
    );
    assert!(
        kept.len().is_multiple_of(3),
        "kept prefix ends on a codepoint boundary"
    );
    assert!(kept.len() <= 8, "must not exceed the requested byte budget");
}
