//! Task handle methods.

use crate::{IngotError, IngotHandle, Task};
impl IngotHandle {
    // ── tasks ─────────────────────────────────────────────────────────────────

    /// Inserts a new [`Task`].
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn create_task(&self, task: Task) -> Result<(), IngotError> {
        self.run_blocking(move |ig| ig.create_task(&task)).await
    }

    /// Returns tasks, optionally filtered by `status`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_tasks(&self, status: Option<String>) -> Result<Vec<Task>, IngotError> {
        self.run_blocking(move |ig| ig.list_tasks(status.as_deref()))
            .await
    }

    /// Returns the completed conversation turns for `session_id`, oldest first
    /// (`title` = user message, `response` = assistant reply).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`], or [`IngotError::TaskPanic`] on panic.
    pub async fn session_history(&self, session_id: &str) -> Result<Vec<Task>, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.session_history(&session_id))
            .await
    }

    /// Updates the `status` field for a task.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn update_task_status(&self, id: &str, status: &str) -> Result<(), IngotError> {
        let id = id.to_owned();
        let status = status.to_owned();
        self.run_blocking(move |ig| ig.update_task_status(&id, &status))
            .await
    }

    /// Retrieves a [`Task`] by `id`, returning `None` when not found.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn get_task(&self, id: &str) -> Result<Option<Task>, IngotError> {
        let id = id.to_owned();
        self.run_blocking(move |ig| ig.get_task(&id)).await
    }

    /// Stores `response` text for a task and sets `status = "complete"`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn set_task_response(&self, id: &str, response: &str) -> Result<(), IngotError> {
        let id = id.to_owned();
        let response = response.to_owned();
        self.run_blocking(move |ig| ig.set_task_response(&id, &response))
            .await
    }
}
