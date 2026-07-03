//! Session handle methods.

use crate::{Ingot, IngotError, IngotHandle, Session};
impl IngotHandle {
    // ── sessions ─────────────────────────────────────────────────────────────

    /// Inserts a new [`Session`].
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn create_session(&self, session: Session) -> Result<(), IngotError> {
        self.run_blocking(move |ig| ig.create_session(&session))
            .await
    }

    /// Retrieves a [`Session`] by `id`, returning `None` when not found.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn get_session(&self, id: &str) -> Result<Option<Session>, IngotError> {
        let id = id.to_owned();
        self.run_blocking(move |ig| ig.get_session(&id)).await
    }

    /// Returns all [`Session`]s ordered by `created_at` ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_sessions(&self) -> Result<Vec<Session>, IngotError> {
        self.run_blocking(Ingot::list_sessions).await
    }

    /// Searches sessions by title or `workspace_root` substring (case-insensitive).
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    pub async fn search_sessions(&self, query: &str) -> Result<Vec<Session>, IngotError> {
        let q = query.to_owned();
        self.run_blocking(move |ingot| ingot.search_sessions(&q))
            .await
    }

    /// Deletes the session with the given `id`.
    ///
    /// Returns `true` if a row was deleted, `false` if none existed.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying DELETE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn delete_session(&self, id: &str) -> Result<bool, IngotError> {
        let id = id.to_owned();
        self.run_blocking(move |ig| ig.delete_session(&id)).await
    }

    /// Updates the `status` of a session.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn update_session_status(&self, id: &str, status: &str) -> Result<(), IngotError> {
        let id = id.to_owned();
        let status = status.to_owned();
        self.run_blocking(move |ig| ig.update_session_status(&id, &status))
            .await
    }

    /// Sets the `workspace_root` path for a session.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn update_session_workspace_root(
        &self,
        session_id: &str,
        workspace_root: &str,
    ) -> Result<(), IngotError> {
        let session_id = session_id.to_owned();
        let workspace_root = workspace_root.to_owned();
        self.run_blocking(move |ig| ig.update_session_workspace_root(&session_id, &workspace_root))
            .await
    }

    /// Sets the `mode` field for a session.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn update_session_mode(
        &self,
        session_id: &str,
        mode: &str,
    ) -> Result<(), IngotError> {
        let session_id = session_id.to_owned();
        let mode = mode.to_owned();
        self.run_blocking(move |ig| ig.update_session_mode(&session_id, &mode))
            .await
    }

    /// Sets the `model_override` for a session.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn update_session_model_override(
        &self,
        session_id: &str,
        model: &str,
    ) -> Result<(), IngotError> {
        let session_id = session_id.to_owned();
        let model = model.to_owned();
        self.run_blocking(move |ig| ig.update_session_model_override(&session_id, &model))
            .await
    }

    /// Sets the `runner_override` for a session.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn update_session_runner_override(
        &self,
        session_id: &str,
        runner: &str,
    ) -> Result<(), IngotError> {
        let session_id = session_id.to_owned();
        let runner = runner.to_owned();
        self.run_blocking(move |ig| ig.update_session_runner_override(&session_id, &runner))
            .await
    }

    /// Links a session to a task.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn update_session_task_id(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<(), IngotError> {
        let session_id = session_id.to_owned();
        let task_id = task_id.to_owned();
        self.run_blocking(move |ig| ig.update_session_task_id(&session_id, &task_id))
            .await
    }

    /// Enables or disables the cowork gate for a session.
    ///
    /// This is the single canonical cowork-mode setter; it also refreshes the
    /// session's `updated_at` timestamp.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn update_session_cowork_mode(
        &self,
        session_id: &str,
        enabled: bool,
    ) -> Result<(), IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.update_session_cowork_mode(&session_id, enabled))
            .await
    }

    /// Sets the human-readable `title` for a session.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn update_session_title(
        &self,
        session_id: &str,
        title: &str,
    ) -> Result<(), IngotError> {
        let session_id = session_id.to_owned();
        let title = title.to_owned();
        self.run_blocking(move |ig| ig.update_session_title(&session_id, &title))
            .await
    }
}
