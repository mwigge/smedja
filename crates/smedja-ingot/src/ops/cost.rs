//! Cost ledger, metrics/savings rollups, and token-snapshot operations.

use crate::{
    cost, metrics_rollup, savings_rollup, token_snapshot, CostEntry, CostRow, Ingot, IngotError,
    MetricsBucket, SavingsBucket, SavingsSummary, TokenSnapshot, TokensSavedEntry,
};
impl Ingot {
    // cost_ledger ------------------------------------------------------------

    /// Appends a [`CostEntry`] to the cost ledger.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails.
    #[must_use = "check the Result to confirm the cost entry was recorded"]
    pub fn insert_cost(&self, entry: &CostEntry) -> Result<(), IngotError> {
        cost::insert(&self.conn, entry)
    }

    /// Returns the exact total cost (microdollars) for all entries in
    /// `session_id`.
    ///
    /// Returns `Microdollars::from_micros(0)` when no entries exist.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned sum"]
    pub fn session_cost(&self, session_id: &str) -> Result<smedja_types::Microdollars, IngotError> {
        cost::session_total(&self.conn, session_id)
    }

    /// Returns per-model/runner aggregate rows for `session_id`, sorted by descending cost.
    ///
    /// Returns an empty vec when no entries exist.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned rows"]
    pub fn session_cost_entries(&self, session_id: &str) -> Result<Vec<CostRow>, IngotError> {
        cost::session_cost_entries(&self.conn, session_id)
    }

    /// Returns the model name from the most recent cost entry for `session_id`.
    ///
    /// Returns `None` when no cost entries exist.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result to determine the active model"]
    pub fn session_last_model(&self, session_id: &str) -> Result<Option<String>, IngotError> {
        cost::last_model(&self.conn, session_id)
    }

    /// Records a [`TokensSavedEntry`] on the tokens-saved ledger.
    ///
    /// Savings are kept separate from the billed `cost_ledger` so billed totals
    /// stay exact.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails.
    #[must_use = "check the Result to confirm the tokens-saved entry was recorded"]
    pub fn insert_tokens_saved(&self, entry: &TokensSavedEntry) -> Result<(), IngotError> {
        cost::insert_tokens_saved(&self.conn, entry)
    }

    /// Returns the total tokens saved by filtering for `session_id`.
    ///
    /// Returns `0` when no entries exist.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned total"]
    pub fn session_tokens_saved(&self, session_id: &str) -> Result<i64, IngotError> {
        cost::session_tokens_saved(&self.conn, session_id)
    }

    /// Returns the sum of `tokens_saved` grouped by `source` for `session_id`,
    /// ordered by `source`.
    ///
    /// Each tuple is `(source, summed_tokens_saved)`. Cache savings
    /// (`source = 'cache'`) stay distinct from compression savings.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned per-source sums"]
    pub fn session_tokens_saved_by_source(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, i64)>, IngotError> {
        cost::session_tokens_saved_by_source(&self.conn, session_id)
    }

    // metrics_rollups --------------------------------------------------------

    /// Computes time-tiered metrics buckets for `tier` over `[since, until)`.
    ///
    /// Aggregates tokens, cost, and turns from `cost_ledger` and error counts
    /// from `audit_events` (`status = 'error'`) per `(bucket, runner)`, merging
    /// the two on `(bucket_start, runner)`. Buckets are computed on read from the
    /// source rows — there is no staleness and no background writer. Results are
    /// ordered by `bucket_start` then `runner`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if either source query fails.
    #[must_use = "check the Result and inspect the returned buckets"]
    pub fn metrics_rollup(
        &self,
        tier: metrics_rollup::RollupTier,
        since: smedja_types::Timestamp,
        until: smedja_types::Timestamp,
    ) -> Result<Vec<MetricsBucket>, IngotError> {
        metrics_rollup::compute(&self.conn, tier, since, until)
    }

    /// Upserts the computed buckets for `tier` over `[epoch, until)` into the
    /// `metrics_rollups` cache, keyed on `(tier, bucket_start, runner)`.
    ///
    /// Materialises every bucket up to (but not including) `until`. Idempotent:
    /// re-running with the same `until` reproduces identical rows, and the stored
    /// rows equal `metrics_rollup(tier, epoch, until)`. The returned buckets are
    /// exactly what was stored.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the source queries or the upsert fail.
    #[must_use = "check the Result to confirm the rollups were materialised"]
    pub fn materialise_rollups(
        &self,
        tier: metrics_rollup::RollupTier,
        until: smedja_types::Timestamp,
    ) -> Result<Vec<MetricsBucket>, IngotError> {
        metrics_rollup::materialise(
            &self.conn,
            tier,
            smedja_types::Timestamp::from_micros(0),
            until,
        )
    }

    // savings_rollup ---------------------------------------------------------

    /// Computes time-tiered savings buckets for `tier` over `[since, until)`.
    ///
    /// Aggregates `tokens_saved` from `tokens_saved_ledger` per
    /// `(bucket, source)`, reusing [`RollupTier::bucket_start`] so savings
    /// buckets align with the billed buckets in [`Self::metrics_rollup`].
    /// Results are ordered by `bucket_start` then `source`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the source query fails.
    #[must_use = "check the Result and inspect the returned buckets"]
    pub fn savings_rollup(
        &self,
        tier: metrics_rollup::RollupTier,
        since: smedja_types::Timestamp,
        until: smedja_types::Timestamp,
    ) -> Result<Vec<SavingsBucket>, IngotError> {
        savings_rollup::compute(&self.conn, tier, since, until)
    }

    /// Computes the efficiency ratio `saved / (saved + billed_input)` over
    /// `[since, until)`.
    ///
    /// `saved` is the all-source `tokens_saved` sum; `billed_input` is the
    /// `cost_ledger.input_tok` sum over the same range. Returns `0.0` for an
    /// empty window. The `tier` argument is accepted for surface symmetry with
    /// [`Self::savings_rollup`]; the ratio is computed over the whole window.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if either source query fails.
    #[must_use = "check the Result and inspect the returned ratio"]
    pub fn efficiency_ratio(
        &self,
        tier: metrics_rollup::RollupTier,
        since: smedja_types::Timestamp,
        until: smedja_types::Timestamp,
    ) -> Result<f64, IngotError> {
        let _ = tier;
        savings_rollup::efficiency_ratio(&self.conn, since, until)
    }

    /// Computes the full [`SavingsSummary`] for `tier` over `[since, until)`.
    ///
    /// Carries the per-`(bucket, source)` rows plus the headline split:
    /// compression total (`filter` + `crusher` + `cold-context`) and cache total
    /// kept as separate figures, never summed, and the efficiency ratio.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if any source query fails.
    #[must_use = "check the Result and inspect the returned summary"]
    pub fn savings_summary(
        &self,
        tier: metrics_rollup::RollupTier,
        since: smedja_types::Timestamp,
        until: smedja_types::Timestamp,
    ) -> Result<SavingsSummary, IngotError> {
        savings_rollup::summary(&self.conn, tier, since, until)
    }

    // token_snapshots --------------------------------------------------------

    /// Saves a [`TokenSnapshot`], replacing any existing snapshot for the same
    /// `(session_id, turn_n)` pair.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the upsert fails.
    #[must_use = "check the Result to confirm the snapshot was saved"]
    pub fn save_token_snapshot(&self, snap: &TokenSnapshot) -> Result<(), IngotError> {
        token_snapshot::save(&self.conn, snap)
    }

    /// Returns all [`TokenSnapshot`]s for `session_id`, ordered by `turn_n` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned snapshots"]
    pub fn session_token_snapshots(
        &self,
        session_id: &str,
    ) -> Result<Vec<TokenSnapshot>, IngotError> {
        token_snapshot::list_by_session(&self.conn, session_id)
    }
}
