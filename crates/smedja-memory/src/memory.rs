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
#[derive(Debug)]
pub struct WorkingMemory {
    messages: Vec<Message>,
    /// Number of leading messages frozen against compaction and reordering.
    /// Guards the provider KV-cache prefix.
    stable_prefix: usize,
    /// Soft limit used by `build_prompt` to budget context.
    max_tokens: usize,
    /// Per-tier context window boundaries for hot/warm/cold strata.
    strata: StrataConfig,
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
        }
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
            hot_depth: 5,
            warm_depth: 30,
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
    use crate::types::{Message, Stratum};

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
}
