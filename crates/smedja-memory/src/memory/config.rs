//! Configuration types for [`WorkingMemory`](super::WorkingMemory): retention
//! strata boundaries and cold-store query parameters.

/// Hot window size: the last `HOT_WINDOW` turns are always included verbatim.
pub const HOT_WINDOW: usize = 5;

/// Warm window size: turns within `WARM_WINDOW` positions from the end are
/// included in context when the token budget allows, after the hot window.
pub const WARM_WINDOW: usize = 30;

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

/// Per-tier context window boundaries for hot/warm/cold strata.
///
/// Callers set this once at session start via [`WorkingMemory::set_strata`];
/// subsequent calls to [`WorkingMemory::stratum_for`] use the configured values.
///
/// [`WorkingMemory::set_strata`]: super::WorkingMemory::set_strata
/// [`WorkingMemory::stratum_for`]: super::WorkingMemory::stratum_for
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
