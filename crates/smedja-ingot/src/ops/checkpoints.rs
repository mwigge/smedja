//! Checkpoint operations.

use crate::{checkpoint, Checkpoint, Ingot, IngotError};
impl Ingot {
    // checkpoints ------------------------------------------------------------

    /// Saves a [`Checkpoint`], replacing any existing checkpoint for the same
    /// `(session_id, turn_n)` pair.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the upsert fails.
    #[must_use = "check the Result to confirm the checkpoint was saved"]
    pub fn save_checkpoint(&self, cp: &Checkpoint) -> Result<(), IngotError> {
        checkpoint::save(&self.conn, cp)
    }

    /// Loads the [`Checkpoint`] for `(session_id, turn_n)`, returning `None` if not found.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned checkpoint"]
    pub fn load_checkpoint(
        &self,
        session_id: &str,
        turn_n: u32,
    ) -> Result<Option<Checkpoint>, IngotError> {
        checkpoint::load(&self.conn, session_id, turn_n)
    }

    /// Returns the checkpoint with the highest `turn_n` for `session_id`, or `None`
    /// if no checkpoints exist.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned checkpoint"]
    pub fn latest_checkpoint(&self, session_id: &str) -> Result<Option<Checkpoint>, IngotError> {
        checkpoint::latest(&self.conn, session_id)
    }

    /// Returns all ordinary (non-compaction) checkpoints for `session_id`,
    /// ordered by turn number ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned checkpoints"]
    pub fn list_checkpoints(&self, session_id: &str) -> Result<Vec<Checkpoint>, IngotError> {
        checkpoint::list(&self.conn, session_id)
    }

    /// Returns all compaction checkpoints for `session_id`, ordered by
    /// `created_at` ascending. Each carries a distinct `compaction_id`, so a
    /// session retains every compaction rather than overwriting the previous one.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned compaction checkpoints"]
    pub fn list_compaction_checkpoints(
        &self,
        session_id: &str,
    ) -> Result<Vec<Checkpoint>, IngotError> {
        checkpoint::list_compactions(&self.conn, session_id)
    }

    /// Atomically rolls back a session to `turn_n`, pruning all later checkpoints.
    ///
    /// Loads the checkpoint at `turn_n` and, within the same `SQLite` transaction,
    /// deletes every checkpoint for `session_id` with a turn number greater than
    /// `turn_n`. Returns `Ok(Some(checkpoint))` on success, or `Ok(None)` when no
    /// checkpoint exists at the requested turn (no rows are modified in that case).
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if any SQL operation fails.
    #[must_use = "check the Result to confirm the rollback succeeded"]
    pub fn rollback_session(
        &self,
        session_id: &str,
        turn_n: u32,
    ) -> Result<Option<Checkpoint>, IngotError> {
        checkpoint::rollback_session(&self.conn, session_id, turn_n)
    }
}
