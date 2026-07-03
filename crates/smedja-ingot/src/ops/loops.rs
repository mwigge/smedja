//! Loop, methodology, and prompt-hash operations.

use crate::{
    loop_state, methodology, now_epoch, prompt_hash, Ingot, IngotError, LoopRecord,
    MethodologyState, PromptHashRecord,
};
impl Ingot {
    // loops ------------------------------------------------------------------

    /// Inserts a new [`LoopRecord`] into the `loops` table.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails (e.g. duplicate `id`).
    #[must_use = "check the Result to confirm the loop record was created"]
    pub fn create_loop(&self, rec: &LoopRecord) -> Result<(), IngotError> {
        loop_state::insert(&self.conn, rec)
    }

    /// Returns the spec-first methodology state for `session_id`, or the
    /// all-false default when no row exists.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned methodology state"]
    pub fn get_methodology_state(&self, session_id: &str) -> Result<MethodologyState, IngotError> {
        methodology::get(&self.conn, session_id)
    }

    /// Sets the `spec_recorded` flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPSERT fails.
    #[must_use = "check the Result to confirm the flag was set"]
    pub fn set_spec_recorded(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        methodology::set_spec_recorded(&self.conn, session_id, value)
    }

    /// Sets the `approval_recorded` flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPSERT fails.
    #[must_use = "check the Result to confirm the flag was set"]
    pub fn set_approval_recorded(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        methodology::set_approval_recorded(&self.conn, session_id, value)
    }

    /// Sets the per-session `no_spec_gate` escape-hatch flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPSERT fails.
    #[must_use = "check the Result to confirm the flag was set"]
    pub fn set_no_spec_gate(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        methodology::set_no_spec_gate(&self.conn, session_id, value)
    }

    /// Retrieves a [`LoopRecord`] by `id`, returning `None` when not found.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned loop record"]
    pub fn get_loop(&self, id: &str) -> Result<Option<LoopRecord>, IngotError> {
        loop_state::get(&self.conn, id)
    }

    /// Updates the `status` and `updated_at` fields for the loop with `id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the loop status was updated"]
    pub fn update_loop_status(
        &self,
        id: &str,
        status: &str,
        updated_at: smedja_types::Timestamp,
    ) -> Result<(), IngotError> {
        loop_state::update_status(&self.conn, id, status, updated_at)
    }

    /// Returns all [`LoopRecord`]s for `change_name`, ordered by `created_at` descending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned loop records"]
    pub fn list_loops(&self, change_name: &str) -> Result<Vec<LoopRecord>, IngotError> {
        loop_state::list_by_change(&self.conn, change_name)
    }

    /// Updates `current_slice` and `updated_at` for the loop with `id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the slice was updated"]
    pub fn update_loop_slice(
        &self,
        id: &str,
        current_slice: i64,
        updated_at: smedja_types::Timestamp,
    ) -> Result<(), IngotError> {
        loop_state::update_slice(&self.conn, id, current_slice, updated_at)
    }

    /// Returns all [`LoopRecord`]s, optionally filtered by `status`.
    ///
    /// Pass `None` to return all loops. Pass `Some("retired")` (or any other valid
    /// status string) to restrict the result set. Results are ordered by
    /// `created_at` descending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned loop records"]
    pub fn list_loops_by_status(
        &self,
        status: Option<&str>,
    ) -> Result<Vec<LoopRecord>, IngotError> {
        loop_state::list_by_status(&self.conn, status)
    }

    // prompt_hashes ----------------------------------------------------------

    /// Records a prompt content hash for `(change, role)` at the current time.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails.
    #[must_use = "check the Result to confirm the hash was saved"]
    pub fn save_prompt_hash(&self, change: &str, role: &str, hash: &str) -> Result<(), IngotError> {
        prompt_hash::save(&self.conn, change, role, hash, now_epoch())
    }

    /// Returns the most recent prompt hash for `(change, role)`, or `None` when
    /// no record exists.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned hash"]
    pub fn get_prompt_hash(&self, change: &str, role: &str) -> Result<Option<String>, IngotError> {
        prompt_hash::get_latest(&self.conn, change, role)
    }

    /// Returns all prompt hash records for `change`, ordered by `ts` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned records"]
    pub fn list_prompt_hashes(&self, change: &str) -> Result<Vec<PromptHashRecord>, IngotError> {
        prompt_hash::list_by_change(&self.conn, change)
    }
}
