//! Per-tier context window boundaries for the hot/warm/cold strata.

use super::{HOT_WINDOW, WARM_WINDOW};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
