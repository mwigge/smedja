//! Provider-session bookkeeping and context-pressure helpers used by the turn
//! orchestrator: the shared provider-resume and cache-aligner maps, their GC,
//! and the auto-compaction threshold predicates.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

/// A provider-native resume identifier plus the last time it was read or written.
///
/// The `last_used` stamp lets the background GC evict only genuinely idle entries
/// and never wipe a session whose turn is running right now.
#[derive(Debug, Clone)]
pub(crate) struct ProviderSessionEntry {
    /// The provider-native resume id.
    pub id: String,
    /// When this entry was last read or inserted.
    pub last_used: std::time::Instant,
}

impl ProviderSessionEntry {
    /// Wraps a resume `id`, stamping it as used now.
    pub fn new(id: String) -> Self {
        Self {
            id,
            last_used: std::time::Instant::now(),
        }
    }
}

/// Shared map from session-resume keys to provider-native resume identifiers.
///
/// Constructed once in `main()` and threaded explicitly to every orchestrator
/// (replacing the former process-static `OnceLock` singleton) so tests can
/// supply their own map.
pub(crate) type ProviderSessions = Arc<Mutex<HashMap<String, ProviderSessionEntry>>>;

/// Evicts idle entries from the provider-session `map` once it exceeds `cap`.
///
/// Only entries not read or written within `idle` are removed; a session whose
/// turn touched its entry more recently than `idle` is retained even over `cap`,
/// so in-flight turns never lose their provider-native resume id. Returns the
/// number of entries evicted.
pub(crate) fn gc_provider_sessions(
    map: &mut HashMap<String, ProviderSessionEntry>,
    cap: usize,
    idle: std::time::Duration,
) -> usize {
    if map.len() <= cap {
        return 0;
    }
    let now = std::time::Instant::now();
    let before = map.len();
    map.retain(|_, entry| now.duration_since(entry.last_used) < idle);
    before - map.len()
}

/// Key identifying a persisted [`smedja_memory::CacheAligner`]: `(session_id, runner_name)`.
///
/// Keyed by runner as well as session because a [`smedja_memory::CacheHint`]
/// targets one specific provider's warm cache; a `provider-failover` runner
/// rotation must not smear one provider's prefix-digest history onto another.
pub(crate) type AlignerKey = (String, String);

/// Shared map from `(session_id, runner)` to its persisted cross-turn aligner.
///
/// Constructed once in `main()` and threaded to every orchestrator exactly like
/// [`ProviderSessions`], so a single aligner instance outlives an individual turn
/// and can observe the prior sealed prefix to report real `Grown`/`Mutated` drift.
pub(crate) type CacheAligners = Arc<Mutex<HashMap<AlignerKey, smedja_memory::CacheAligner>>>;

/// Returns the auto-compact threshold from `val` (an optional env value string), defaulting to
/// 0.85. Values below 0.5 are clamped to 0.5 to prevent spurious compaction.
pub(crate) fn compact_threshold_from_env(val: Option<&str>) -> f64 {
    val.and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.85)
        .max(0.5)
}

/// Returns `true` when context fill exceeds the auto-summarisation threshold.
pub(crate) fn context_pressure_exceeds_threshold(
    input_tokens: u32,
    context_window: usize,
    threshold: f64,
) -> bool {
    if context_window == 0 {
        return false;
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = f64::from(input_tokens) / context_window as f64;
    ratio >= threshold
}
