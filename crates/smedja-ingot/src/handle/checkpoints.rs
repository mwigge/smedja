//! Checkpoint handle methods.

use crate::{Checkpoint, IngotError, IngotHandle};
impl IngotHandle {
    // ── checkpoints ───────────────────────────────────────────────────────────

    /// Saves a [`Checkpoint`]. Ordinary per-turn checkpoints replace any existing
    /// one for the same `(session_id, turn_n)`; compaction checkpoints are retained.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying upsert, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn save_checkpoint(&self, cp: Checkpoint) -> Result<(), IngotError> {
        self.run_blocking(move |ig| ig.save_checkpoint(&cp)).await
    }

    /// Loads the ordinary [`Checkpoint`] for `(session_id, turn_n)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn load_checkpoint(
        &self,
        session_id: &str,
        turn_n: u32,
    ) -> Result<Option<Checkpoint>, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.load_checkpoint(&session_id, turn_n))
            .await
    }

    /// Returns the ordinary checkpoint with the highest `turn_n` for `session_id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn latest_checkpoint(
        &self,
        session_id: &str,
    ) -> Result<Option<Checkpoint>, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.latest_checkpoint(&session_id))
            .await
    }

    /// Returns all ordinary checkpoints for `session_id`, ordered by turn number ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_checkpoints(&self, session_id: &str) -> Result<Vec<Checkpoint>, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.list_checkpoints(&session_id))
            .await
    }

    /// Returns all compaction checkpoints for `session_id`, ordered by
    /// `created_at` ascending. Every compaction is retained.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_compaction_checkpoints(
        &self,
        session_id: &str,
    ) -> Result<Vec<Checkpoint>, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.list_compaction_checkpoints(&session_id))
            .await
    }

    /// Atomically rolls back a session to `turn_n`, pruning later checkpoints.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from any SQL operation, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn rollback_session(
        &self,
        session_id: &str,
        turn_n: u32,
    ) -> Result<Option<Checkpoint>, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.rollback_session(&session_id, turn_n))
            .await
    }
}
