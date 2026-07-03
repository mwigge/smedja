//! Task operations.

use crate::{task, Ingot, IngotError, Task};
impl Ingot {
    // tasks ------------------------------------------------------------------

    /// Inserts a new [`Task`].
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails.
    #[must_use = "check the Result to confirm the task was created"]
    pub fn create_task(&self, task: &Task) -> Result<(), IngotError> {
        task::create(&self.conn, task)
    }

    /// Returns tasks, optionally filtered by `status`.
    ///
    /// Pass `None` to return all tasks. Pass `Some("planned")` (or any other valid
    /// status string) to restrict the result set.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned tasks"]
    pub fn list_tasks(&self, status: Option<&str>) -> Result<Vec<Task>, IngotError> {
        task::list(&self.conn, status)
    }

    /// Returns the completed conversation turns for `session_id`, oldest first
    /// (each task's `title` is the user message, `response` the assistant reply).
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    pub fn session_history(&self, session_id: &str) -> Result<Vec<Task>, IngotError> {
        task::history_for_session(&self.conn, session_id)
    }

    /// Updates the `status` field for the task identified by `id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the status was updated"]
    pub fn update_task_status(&self, id: &str, status: &str) -> Result<(), IngotError> {
        task::update_status(&self.conn, id, status)
    }

    /// Retrieves a [`Task`] by `id`, returning `None` when not found.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned task"]
    pub fn get_task(&self, id: &str) -> Result<Option<Task>, IngotError> {
        task::get(&self.conn, id)
    }

    /// Stores `response` text for the task identified by `id` and sets
    /// `status = "complete"` in the same UPDATE.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the response was stored"]
    pub fn set_task_response(&self, id: &str, response: &str) -> Result<(), IngotError> {
        task::update_response(&self.conn, id, response)
    }
}
