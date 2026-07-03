//! JSONL export/import and maintenance handle methods.

use crate::{Ingot, IngotError, IngotHandle};
impl IngotHandle {
    // ── JSONL export / import ─────────────────────────────────────────────────

    /// Exports tasks and their associated audit events as a JSONL stream.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] or [`IngotError::Json`] from the underlying
    /// export logic, or [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn export_jsonl(
        &self,
        change: Option<String>,
    ) -> Result<Vec<serde_json::Value>, IngotError> {
        self.run_blocking(move |ig| ig.export_jsonl(change.as_deref()))
            .await
    }

    /// Imports tasks and audit events from a JSONL stream.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Json`] or [`IngotError::Db`] from the underlying
    /// import logic, or [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn import_jsonl(&self, records: Vec<serde_json::Value>) -> Result<usize, IngotError> {
        self.run_blocking(move |ig| ig.import_jsonl(&records)).await
    }

    /// Deletes old terminated sessions and orphaned dependent rows.
    /// See [`Ingot::prune_old_sessions`].
    ///
    /// # Errors
    ///
    /// Returns [`IngotError`] on database failure.
    pub async fn prune_old_sessions(&self, older_than_days: u64) -> Result<usize, IngotError> {
        self.run_blocking(move |ig| ig.prune_old_sessions(older_than_days))
            .await
    }

    /// Checkpoints the WAL and rebuilds the database to reclaim space.
    /// See [`Ingot::vacuum`].
    ///
    /// # Errors
    ///
    /// Returns [`IngotError`] on database failure.
    pub async fn vacuum(&self) -> Result<(), IngotError> {
        self.run_blocking(Ingot::vacuum).await
    }
}
