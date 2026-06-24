use std::sync::Arc;

use crate::cold::ColdStore;
use crate::types::{Message, Stratum};

/// Hot window size: the last `HOT_WINDOW` turns are always included verbatim.
pub const HOT_WINDOW: usize = 5;

/// Warm window size: turns within `WARM_WINDOW` positions from the end are
/// included in context when the token budget allows, after the hot window.
pub const WARM_WINDOW: usize = 30;

/// In-memory working context for a single agent session.
///
/// Holds the ordered list of conversation messages, a stable-prefix boundary
/// that guards the provider KV-cache, and a soft token budget used by prompt
/// assembly.
///
/// The hot/warm/cold strata boundaries default to [`StrataConfig::deep`]
/// (hot=5, warm=30) and can be reconfigured via [`WorkingMemory::set_strata`].
///
/// An optional [`ColdStore`] can be attached via [`WorkingMemory::with_cold_store`]
/// to enable semantic recall of cold-stratum context through
/// [`WorkingMemory::cold_context`].
pub struct WorkingMemory {
    messages: Vec<Message>,
    /// Number of leading messages frozen against compaction and reordering.
    /// Guards the provider KV-cache prefix.
    stable_prefix: usize,
    /// Soft limit used by `build_prompt` to budget context.
    max_tokens: usize,
    /// Per-tier context window boundaries for hot/warm/cold strata.
    strata: StrataConfig,
    /// Optional cold-stratum retrieval port. `None` means cold recall is
    /// disabled and [`WorkingMemory::cold_context`] returns an empty `Vec`.
    cold_store: Option<Arc<dyn ColdStore>>,
    /// Namespace and top-K used when querying the cold store.
    cold_query: ColdQuery,
}

impl std::fmt::Debug for WorkingMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkingMemory")
            .field("messages", &self.messages)
            .field("stable_prefix", &self.stable_prefix)
            .field("max_tokens", &self.max_tokens)
            .field("strata", &self.strata)
            .field(
                "cold_store",
                &self.cold_store.as_ref().map(|_| "<ColdStore>"),
            )
            .field("cold_query", &self.cold_query)
            .finish()
    }
}

/// Namespace and top-K configuration for cold-store queries.
///
/// Defaults to the `"compact"` namespace (where `session.compact` indexes its
/// summaries) and `k = 3`.
#[derive(Debug, Clone)]
pub struct ColdQuery {
    /// Vault namespace to search.
    pub namespace: String,
    /// Maximum number of cold results to retrieve.
    pub k: usize,
}

impl Default for ColdQuery {
    fn default() -> Self {
        Self {
            namespace: "compact".to_owned(),
            k: 3,
        }
    }
}

impl WorkingMemory {
    /// Creates a new, empty [`WorkingMemory`] with the given soft token limit.
    ///
    /// The strata configuration defaults to [`StrataConfig::deep`] (hot=5, warm=30).
    /// Use [`WorkingMemory::set_strata`] to switch to a different preset.
    #[must_use]
    pub fn new(max_tokens: usize) -> Self {
        Self {
            messages: Vec::new(),
            stable_prefix: 0,
            max_tokens,
            strata: StrataConfig::deep(),
            cold_store: None,
            cold_query: ColdQuery::default(),
        }
    }

    /// Attaches a [`ColdStore`] for semantic recall and returns `self`.
    ///
    /// Without a store attached, [`WorkingMemory::cold_context`] returns an
    /// empty `Vec`. The cold-query config retains its default
    /// (`namespace = "compact"`, `k = 3`); adjust it with
    /// [`WorkingMemory::set_cold_query`].
    #[must_use]
    pub fn with_cold_store(mut self, store: Arc<dyn ColdStore>) -> Self {
        self.cold_store = Some(store);
        self
    }

    /// Sets the namespace and top-K used by [`WorkingMemory::cold_context`].
    pub fn set_cold_query(&mut self, namespace: impl Into<String>, k: usize) {
        self.cold_query = ColdQuery {
            namespace: namespace.into(),
            k,
        };
    }

    /// Pushes a message onto the working memory.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `stable_prefix` has already been sealed and
    /// the caller attempts to insert into the frozen region (i.e. the message
    /// count at the time of push is less than `stable_prefix`). The stable
    /// prefix is always set once at session start; growing it via push after
    /// sealing is a programming error.
    pub fn push(&mut self, msg: Message) {
        debug_assert!(
            self.messages.len() >= self.stable_prefix,
            "push would violate stable prefix: len={} prefix={}",
            self.messages.len(),
            self.stable_prefix,
        );
        self.messages.push(msg);
    }

    /// Freezes the current message count as the stable prefix.
    ///
    /// Call this exactly once, after injecting the system prompt, skills, and
    /// code-graph context that must survive unchanged for the provider to reuse
    /// its KV-cache.
    pub fn seal_prefix(&mut self) {
        self.stable_prefix = self.messages.len();
        tracing::debug!(stable_prefix = self.stable_prefix, "prefix sealed");
    }

    /// Returns the stable prefix boundary (number of frozen leading messages).
    #[must_use]
    pub fn stable_prefix(&self) -> usize {
        self.stable_prefix
    }

    /// Returns the total number of messages in working memory.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Returns `true` when working memory contains no messages.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Returns all messages in order — the full prompt slice.
    ///
    /// Callers (smdjad) are responsible for token budgeting against
    /// `max_tokens`.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Returns only the messages from `stable_prefix` onward — the mutable
    /// window that the compactor is allowed to modify.
    #[must_use]
    pub fn mutable_window(&self) -> &[Message] {
        &self.messages[self.stable_prefix..]
    }

    /// Returns the soft token budget for this session.
    #[must_use]
    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    /// Returns the active strata configuration.
    #[must_use]
    pub fn strata(&self) -> StrataConfig {
        self.strata
    }

    /// Replaces the strata configuration.
    ///
    /// Takes effect immediately; subsequent calls to [`WorkingMemory::stratum_for`]
    /// use the new boundaries.
    pub fn set_strata(&mut self, config: StrataConfig) {
        self.strata = config;
    }

    /// Determines the [`Stratum`] for the message at absolute index `i`.
    ///
    /// - [`Stratum::Hot`]  — last `strata.hot_depth` turns from the end.
    /// - [`Stratum::Warm`] — within `strata.warm_depth` turns from the end (after hot).
    /// - [`Stratum::Cold`] — beyond `strata.warm_depth` turns from the end.
    ///
    /// [`Stratum::Archive`] is not applicable to in-memory messages; it applies
    /// only to completed sessions stored in smedja-ingot.
    #[must_use]
    pub fn stratum_for(&self, index: usize) -> Stratum {
        let len = self.messages.len();
        if len == 0 || index >= len {
            return Stratum::Cold;
        }
        let from_end = len - 1 - index;
        if from_end < self.strata.hot_depth {
            Stratum::Hot
        } else if from_end < self.strata.warm_depth {
            Stratum::Warm
        } else {
            Stratum::Cold
        }
    }

    /// Replaces the mutable window (messages after `stable_prefix`) with
    /// `compacted`.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `compacted` is shorter than the stable prefix,
    /// which would imply the prefix itself has been removed.
    pub fn replace_mutable(&mut self, compacted: Vec<Message>) {
        debug_assert!(
            self.messages.len() >= self.stable_prefix || compacted.len() >= self.stable_prefix,
            "replacement would shrink below stable prefix"
        );
        self.messages.truncate(self.stable_prefix);
        self.messages.extend(compacted);
    }

    /// Assembles the prompt slice respecting the active strata configuration.
    ///
    /// - Stable prefix (system prompt, skills) is always included verbatim.
    /// - Hot turns (last `strata.hot_depth`) are always included verbatim.
    /// - Warm turns are included until `budget_tokens` is exhausted.
    ///   Token count is estimated as `content.len() / 4 + 1` per message.
    /// - Cold turns are omitted (cold retrieval is a future extension).
    ///
    /// The returned slice always starts with the stable prefix.
    #[must_use]
    pub fn build_prompt(&self, budget_tokens: usize) -> Vec<Message> {
        if self.messages.is_empty() {
            return Vec::new();
        }

        let prefix = &self.messages[..self.stable_prefix];
        let mutable = &self.messages[self.stable_prefix..];

        let mut result: Vec<Message> = prefix.to_vec();
        let mut budget = budget_tokens;

        for (i, msg) in mutable.iter().enumerate() {
            let abs_index = self.stable_prefix + i;
            let stratum = self.stratum_for(abs_index);
            let token_estimate = msg.content.len() / 4 + 1;
            match stratum {
                Stratum::Hot => {
                    result.push(msg.clone());
                }
                Stratum::Warm => {
                    if budget >= token_estimate {
                        budget = budget.saturating_sub(token_estimate);
                        result.push(msg.clone());
                    }
                }
                Stratum::Cold | Stratum::Archive => {
                    // ponytail: cold retrieval via cold_context() deferred; skip for now
                }
            }
        }
        result
    }

    /// Returns messages from the cold stratum that are relevant to `query`.
    ///
    /// Cold retrieval uses semantic similarity between `query` and durably
    /// stored content to surface context from beyond the warm window. The query
    /// is dispatched to the attached [`ColdStore`] over the configured
    /// [`ColdQuery`] namespace and top-K; each result is mapped to a system
    /// [`Message`] in the store's descending-relevance order.
    ///
    /// When no cold store is attached the result is an empty `Vec`, preserving
    /// the behaviour of callers (such as strata unit tests) that never opt in.
    pub async fn cold_context(&self, query: &str) -> Vec<crate::types::Message> {
        let Some(store) = &self.cold_store else {
            return Vec::new();
        };
        let results = store
            .retrieve(query, &self.cold_query.namespace, self.cold_query.k)
            .await;
        results
            .into_iter()
            .map(|r| crate::types::Message::system(r.content))
            .collect()
    }
}

/// Loads workspace skill files from `<dir>/.smedja/skills/*.md`.
///
/// Returns an empty [`Vec`] when the directory is absent or no `.md` files
/// are present — this is not an error.
///
/// # Errors
///
/// Returns an error only if the directory exists but cannot be read.
pub fn load_workspace_skills(dir: &std::path::Path) -> Result<Vec<String>, std::io::Error> {
    let skills_dir = dir.join(".smedja").join("skills");
    if !skills_dir.exists() {
        return Ok(Vec::new());
    }
    let mut skills = Vec::new();
    for entry in std::fs::read_dir(&skills_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let content = std::fs::read_to_string(&path)?;
            skills.push(content);
        }
    }
    // Sort for deterministic ordering (alphabetical by filename).
    skills.sort();
    Ok(skills)
}

/// Injects workspace skills into `WorkingMemory` as a single system message
/// before `seal_prefix` is called.
///
/// Skips injection when no skills are found. Returns the number of skills injected.
///
/// # Errors
///
/// Returns an error if the skills directory exists but cannot be read.
pub fn inject_workspace_skills(
    memory: &mut WorkingMemory,
    workspace_dir: &std::path::Path,
) -> Result<usize, std::io::Error> {
    let skills = load_workspace_skills(workspace_dir)?;
    if skills.is_empty() {
        return Ok(0);
    }
    let count = skills.len();
    let combined = skills
        .iter()
        .enumerate()
        .map(|(i, body)| crate::skills::wrap_skill_body(&format!("skill-{i}"), body))
        .collect::<Vec<_>>()
        .join("\n\n");
    memory.push(crate::types::Message::system(format!(
        "[workspace skills]\n\n{combined}"
    )));
    Ok(count)
}

/// Reads `AGENTS.md` from the workspace root, if present.
///
/// Returns `None` when the file is absent — not an error.
///
/// # Errors
///
/// Returns an error only if the file exists but cannot be read.
pub fn detect_agents_md(
    workspace_root: &std::path::Path,
) -> Result<Option<String>, std::io::Error> {
    let path = workspace_root.join("AGENTS.md");
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)?;
    Ok(Some(content))
}

/// Per-tier context window boundaries for hot/warm/cold strata.
///
/// Callers set this once at session start via [`WorkingMemory::set_strata`];
/// subsequent calls to [`WorkingMemory::stratum_for`] use the configured values.
#[derive(Debug, Clone, Copy)]
pub struct StrataConfig {
    /// Number of trailing turns that are always included verbatim.
    pub hot_depth: usize,
    /// Total trailing turns included when the budget allows (warm ≥ hot).
    pub warm_depth: usize,
}

impl StrataConfig {
    /// Fast tier: hot=5, warm=10.
    #[must_use]
    pub fn fast() -> Self {
        Self {
            hot_depth: 5,
            warm_depth: 10,
        }
    }

    /// Deep tier: hot=5, warm=30 (the default).
    #[must_use]
    pub fn deep() -> Self {
        Self {
            hot_depth: HOT_WINDOW,
            warm_depth: WARM_WINDOW,
        }
    }

    /// Local tier: hot=5, warm=15.
    #[must_use]
    pub fn local() -> Self {
        Self {
            hot_depth: 5,
            warm_depth: 15,
        }
    }

    /// Selects a preset from a tier string (`"fast"`, `"deep"`, `"local"`).
    /// Defaults to [`Self::deep`] for unknown strings.
    #[must_use]
    pub fn from_tier(tier: &str) -> Self {
        match tier {
            "fast" => Self::fast(),
            "local" => Self::local(),
            _ => Self::deep(),
        }
    }
}

#[cfg(test)]
mod tests {
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
}
