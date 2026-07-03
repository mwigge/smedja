//! Cost-ledger and tokens-saved handle methods.

use crate::{CostEntry, CostRow, IngotError, IngotHandle, TokensSavedEntry};
use smedja_types::Microdollars;

impl IngotHandle {
    // ── cost_ledger ───────────────────────────────────────────────────────────

    /// Appends a [`CostEntry`] to the cost ledger.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn insert_cost(&self, entry: CostEntry) -> Result<(), IngotError> {
        self.run_blocking(move |ig| ig.insert_cost(&entry)).await
    }

    /// Returns the exact total cost (microdollars) for all entries in `session_id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn session_cost(&self, session_id: &str) -> Result<Microdollars, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.session_cost(&session_id))
            .await
    }

    /// Returns per-model/runner aggregate cost rows for `session_id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn session_cost_entries(&self, session_id: &str) -> Result<Vec<CostRow>, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.session_cost_entries(&session_id))
            .await
    }

    /// Returns the total token count (input + output) attributed to `change_name`
    /// across all audit events. Returns `Ok(0)` when no matching rows exist.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn cost_for_change(&self, change_name: &str) -> Result<u64, IngotError> {
        let change_name = change_name.to_owned();
        self.run_blocking(move |ig| ig.cost_for_change(&change_name))
            .await
    }

    /// Returns the cumulative USD cost (microdollars) for all cost-ledger
    /// entries attributed to `change_name`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn cost_usd_for_change(
        &self,
        change_name: &str,
    ) -> Result<smedja_types::Microdollars, IngotError> {
        let change_name = change_name.to_owned();
        self.run_blocking(move |ig| ig.cost_usd_for_change(&change_name))
            .await
    }

    /// Returns the model name from the most recent cost entry for `session_id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn session_last_model(&self, session_id: &str) -> Result<Option<String>, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.session_last_model(&session_id))
            .await
    }

    /// Records a [`TokensSavedEntry`] on the tokens-saved ledger.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn insert_tokens_saved(&self, entry: TokensSavedEntry) -> Result<(), IngotError> {
        self.run_blocking(move |ig| ig.insert_tokens_saved(&entry))
            .await
    }

    /// Returns the total tokens saved by filtering for `session_id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn session_tokens_saved(&self, session_id: &str) -> Result<i64, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.session_tokens_saved(&session_id))
            .await
    }

    /// Returns the sum of `tokens_saved` grouped by `source` for `session_id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn session_tokens_saved_by_source(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, i64)>, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.session_tokens_saved_by_source(&session_id))
            .await
    }
}
