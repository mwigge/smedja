use std::sync::Arc;

use super::config::{ColdQuery, StrataConfig};
use crate::cold::ColdStore;
use crate::types::{Message, Stratum};

/// Returns the largest byte index `<= max` that lies on a UTF-8 char boundary.
///
/// Slicing a `&str` at an arbitrary byte offset panics when the offset falls in
/// the middle of a multi-byte codepoint (emoji, CJK, accented text). Flooring to
/// the nearest boundary at or below `max` makes `&s[..floor_char_boundary(s, max)]`
/// always safe while never exceeding the requested byte budget.
#[must_use]
pub(crate) fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

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
                        let byte_limit = (budget * 4).min(msg.content.len());
                        // Floor to a UTF-8 char boundary: a raw byte slice at
                        // `byte_limit` can land mid-codepoint (emoji/CJK/accented
                        // text) and panic the whole turn-assembly.
                        let char_limit = floor_char_boundary(&msg.content, byte_limit);
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
