//! Metrics- and savings-rollup handle methods.

use crate::{IngotError, IngotHandle};
use smedja_types::Timestamp;

impl IngotHandle {
    // ── metrics_rollups ───────────────────────────────────────────────────────

    /// Computes time-tiered metrics buckets for `tier` over `[since, until)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying queries, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn metrics_rollup(
        &self,
        tier: crate::RollupTier,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<crate::MetricsBucket>, IngotError> {
        self.run_blocking(move |ig| ig.metrics_rollup(tier, since, until))
            .await
    }

    /// Upserts the computed buckets for `tier` over `[epoch, until)` into the
    /// `metrics_rollups` cache. Idempotent.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying queries or upsert, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn materialise_rollups(
        &self,
        tier: crate::RollupTier,
        until: Timestamp,
    ) -> Result<Vec<crate::MetricsBucket>, IngotError> {
        self.run_blocking(move |ig| ig.materialise_rollups(tier, until))
            .await
    }

    // ── savings_rollup ────────────────────────────────────────────────────────

    /// Computes time-tiered savings buckets for `tier` over `[since, until)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn savings_rollup(
        &self,
        tier: crate::RollupTier,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<crate::SavingsBucket>, IngotError> {
        self.run_blocking(move |ig| ig.savings_rollup(tier, since, until))
            .await
    }

    /// Computes the efficiency ratio `saved / (saved + billed_input)` for `tier`
    /// over `[since, until)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying queries, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn efficiency_ratio(
        &self,
        tier: crate::RollupTier,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<f64, IngotError> {
        self.run_blocking(move |ig| ig.efficiency_ratio(tier, since, until))
            .await
    }

    /// Computes the full [`crate::SavingsSummary`] for `tier` over `[since, until)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying queries, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn savings_summary(
        &self,
        tier: crate::RollupTier,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<crate::SavingsSummary, IngotError> {
        self.run_blocking(move |ig| ig.savings_summary(tier, since, until))
            .await
    }
}
