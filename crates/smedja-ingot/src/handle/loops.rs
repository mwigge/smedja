//! Loop, methodology, and prompt-hash handle methods.

use crate::{IngotError, IngotHandle, LoopRecord};
use smedja_types::Timestamp;

impl IngotHandle {
    // ── loops ─────────────────────────────────────────────────────────────────

    /// Inserts a new [`LoopRecord`].
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn create_loop(&self, rec: LoopRecord) -> Result<(), IngotError> {
        self.run_blocking(move |ig| ig.create_loop(&rec)).await
    }

    /// Returns the spec-first methodology state for `session_id`, or the
    /// all-false default when no row exists.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn get_methodology_state(
        &self,
        session_id: &str,
    ) -> Result<crate::MethodologyState, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.get_methodology_state(&session_id))
            .await
    }

    /// Sets the `spec_recorded` flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn set_spec_recorded(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.set_spec_recorded(&session_id, value))
            .await
    }

    /// Sets the `approval_recorded` flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn set_approval_recorded(
        &self,
        session_id: &str,
        value: bool,
    ) -> Result<(), IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.set_approval_recorded(&session_id, value))
            .await
    }

    /// Sets the per-session `no_spec_gate` escape-hatch flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn set_no_spec_gate(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.set_no_spec_gate(&session_id, value))
            .await
    }

    /// Retrieves a [`LoopRecord`] by `id`, returning `None` when not found.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn get_loop(&self, id: &str) -> Result<Option<LoopRecord>, IngotError> {
        let id = id.to_owned();
        self.run_blocking(move |ig| ig.get_loop(&id)).await
    }

    /// Updates `status` and `updated_at` for a loop.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn update_loop_status(
        &self,
        id: &str,
        status: &str,
        updated_at: Timestamp,
    ) -> Result<(), IngotError> {
        let id = id.to_owned();
        let status = status.to_owned();
        self.run_blocking(move |ig| ig.update_loop_status(&id, &status, updated_at))
            .await
    }

    /// Returns all [`LoopRecord`]s for `change_name`, ordered by `created_at` descending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_loops(&self, change_name: &str) -> Result<Vec<LoopRecord>, IngotError> {
        let change_name = change_name.to_owned();
        self.run_blocking(move |ig| ig.list_loops(&change_name))
            .await
    }

    /// Updates `current_slice` and `updated_at` for a loop.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn update_loop_slice(
        &self,
        id: &str,
        current_slice: i64,
        updated_at: Timestamp,
    ) -> Result<(), IngotError> {
        let id = id.to_owned();
        self.run_blocking(move |ig| ig.update_loop_slice(&id, current_slice, updated_at))
            .await
    }

    /// Returns all [`LoopRecord`]s, optionally filtered by `status`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_loops_by_status(
        &self,
        status: Option<String>,
    ) -> Result<Vec<LoopRecord>, IngotError> {
        self.run_blocking(move |ig| ig.list_loops_by_status(status.as_deref()))
            .await
    }
}
