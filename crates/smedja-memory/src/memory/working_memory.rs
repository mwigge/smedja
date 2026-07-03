//! The in-memory [`WorkingMemory`] message store for a single agent session.

use std::sync::Arc;

use super::StrataConfig;
use crate::cold::ColdStore;
use crate::types::{Message, Stratum};

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
        self.build_prompt_with_omitted(budget_tokens).0
    }

    /// Assembles the budgeted prompt and reports the estimated tokens omitted.
    ///
    /// Returns `(prompt, omitted_tokens)`. `omitted_tokens` is the summed
    /// `content.len() / 4 + 1` estimate for every cold/archive turn dropped and
    /// every warm turn that did not fit the budget — the cold-context saving the
    /// caller may record on the savings ledger (`source = "cold-context"`).
    ///
    /// This crate holds no database handle by design, so recording the saving is
    /// left to the caller that owns one (the orchestrator).
    #[must_use]
    pub fn build_prompt_with_omitted(&self, budget_tokens: usize) -> (Vec<Message>, usize) {
        if self.messages.is_empty() {
            return (Vec::new(), 0);
        }

        let prefix = &self.messages[..self.stable_prefix];
        let mutable = &self.messages[self.stable_prefix..];

        let mut result: Vec<Message> = prefix.to_vec();
        let mut budget = budget_tokens;
        let mut omitted = 0usize;

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
                    } else if budget > 0 {
                        let char_limit = (budget * 4).min(msg.content.len());
                        let mut truncated = msg.clone();
                        truncated.content =
                            format!("{}\n[... truncated]", &msg.content[..char_limit]);
                        result.push(truncated);
                        budget = 0;
                    } else {
                        omitted = omitted.saturating_add(token_estimate);
                    }
                }
                Stratum::Cold | Stratum::Archive => {
                    // Cold retrieval via cold_context() is deferred; the turn is
                    // dropped from the prompt, so its estimate counts as omitted.
                    omitted = omitted.saturating_add(token_estimate);
                }
            }
        }
        (result, omitted)
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
