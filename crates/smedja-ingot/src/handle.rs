//! Async facade over [`Ingot`].
//!
//! [`IngotHandle`] wraps a [`std::sync::Mutex`]-guarded [`Ingot`] in an
//! [`Arc`] so it is cheaply [`Clone`]-able across task boundaries. Every
//! method delegates to the corresponding [`Ingot`] method inside
//! [`tokio::task::spawn_blocking`] via [`IngotHandle::run_blocking`], keeping
//! `SQLite` I/O off the Tokio executor thread-pool.

use std::sync::Arc;

use smedja_types::{Microdollars, Timestamp};

use crate::{
    AuditEvent, Checkpoint, ConversationRollup, CostEntry, CostRow, Ingot, IngotError, LoopRecord,
    McpServer, PromptHashRecord, Session, Task, TokenSnapshot, TokensSavedEntry,
};

/// Converts a [`tokio::task::JoinError`] (a panic inside `spawn_blocking`) into
/// an [`IngotError::TaskPanic`] so callers see a uniform error type.
#[allow(clippy::needless_pass_by_value)] // used as `.map_err(join_err)`, which requires taking the error by value
fn join_err(e: tokio::task::JoinError) -> IngotError {
    IngotError::TaskPanic(e.to_string())
}

/// Async facade over [`Ingot`].
///
/// All methods route through [`tokio::task::spawn_blocking`] so `SQLite`
/// operations do not block Tokio executor threads. The handle is
/// cheaply [`Clone`]able — all clones share the same underlying database
/// connection.
#[derive(Clone)]
pub struct IngotHandle {
    inner: Arc<std::sync::Mutex<Ingot>>,
}

impl IngotHandle {
    /// Wraps `ingot` in an async handle.
    #[must_use]
    pub fn new(ingot: Ingot) -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(ingot)),
        }
    }

    /// Runs `f` against the guarded [`Ingot`] on a blocking thread.
    ///
    /// Clones the shared [`Arc`], moves `f` onto a [`tokio::task::spawn_blocking`]
    /// thread, locks the mutex, and invokes `f` with a shared reference to the
    /// [`Ingot`]. A poisoned lock (left behind by a prior panic) is recovered via
    /// [`std::sync::PoisonError::into_inner`]: the underlying `SQLite` connection
    /// remains valid after a panic because no operation leaves it in a torn
    /// state, so the guard is reused rather than propagating the poison.
    ///
    /// # Errors
    ///
    /// Returns whatever [`IngotError`] `f` produces, or [`IngotError::TaskPanic`]
    /// if the blocking task itself panics.
    async fn run_blocking<T, F>(&self, f: F) -> Result<T, IngotError>
    where
        F: FnOnce(&Ingot) -> Result<T, IngotError> + Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let guard = inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            f(&guard)
        })
        .await
        .map_err(join_err)?
    }

    // ── audit_events ────────────────────────────────────────────────────────

    /// Appends an [`AuditEvent`] to the audit log.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn insert_audit_event(&self, event: AuditEvent) -> Result<(), IngotError> {
        self.run_blocking(move |ig| ig.insert_audit_event(&event))
            .await
    }

    /// Returns all [`AuditEvent`]s for `session_id`, ordered by `ts` ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_audit_events(&self, session_id: &str) -> Result<Vec<AuditEvent>, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.list_audit_events(&session_id))
            .await
    }

    /// Persists a timeline event and upserts the matching [`ConversationRollup`].
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from either the INSERT or the upsert, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn record_timeline_event(&self, event: AuditEvent) -> Result<(), IngotError> {
        self.run_blocking(move |ig| ig.record_timeline_event(&event))
            .await
    }

    /// Returns the most recent `limit` [`ConversationRollup`]s by `last_seen_at` descending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn recent_conversations(
        &self,
        limit: u32,
    ) -> Result<Vec<ConversationRollup>, IngotError> {
        self.run_blocking(move |ig| ig.recent_conversations(limit))
            .await
    }

    /// Returns timeline events for `conversation_id`, ordered by `rowid` ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn conversation_timeline(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<AuditEvent>, IngotError> {
        let conversation_id = conversation_id.to_owned();
        self.run_blocking(move |ig| ig.conversation_timeline(&conversation_id))
            .await
    }

    /// Returns timeline events with `status = 'error'` for `conversation_id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn failed_events(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<AuditEvent>, IngotError> {
        let conversation_id = conversation_id.to_owned();
        self.run_blocking(move |ig| ig.failed_events(&conversation_id))
            .await
    }

    /// Returns all [`AuditEvent`]s, ordered by `ts` ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_all_audit_events(&self) -> Result<Vec<AuditEvent>, IngotError> {
        self.run_blocking(Ingot::list_all_audit_events).await
    }

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

    // ── mcp_servers ──────────────────────────────────────────────────────────

    /// Registers (or replaces) an [`McpServer`].
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT OR REPLACE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn register_mcp_server(&self, server: McpServer) -> Result<(), IngotError> {
        self.run_blocking(move |ig| ig.register_mcp_server(&server))
            .await
    }

    /// Returns all registered [`McpServer`]s ordered by `name` ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_mcp_servers(&self) -> Result<Vec<McpServer>, IngotError> {
        self.run_blocking(Ingot::list_mcp_servers).await
    }

    /// Removes the [`McpServer`] with the given `id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying DELETE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn remove_mcp_server(&self, id: &str) -> Result<(), IngotError> {
        let id = id.to_owned();
        self.run_blocking(move |ig| ig.remove_mcp_server(&id)).await
    }

    /// Updates the cached tool list for a server.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn update_mcp_tools(&self, name: &str, tools_json: &str) -> Result<(), IngotError> {
        let name = name.to_owned();
        let tools_json = tools_json.to_owned();
        self.run_blocking(move |ig| ig.update_mcp_tools(&name, &tools_json))
            .await
    }

    /// Returns stale [`McpServer`]s whose `last_refresh` is older than `older_than_secs`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn get_stale_servers(
        &self,
        older_than_secs: f64,
    ) -> Result<Vec<McpServer>, IngotError> {
        self.run_blocking(move |ig| ig.get_stale_servers(older_than_secs))
            .await
    }

    /// Returns all `(server_name, tools_json)` pairs for servers with non-empty tool lists.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn get_all_mcp_tools(&self) -> Result<Vec<(String, String)>, IngotError> {
        self.run_blocking(Ingot::get_all_mcp_tools).await
    }

    /// Looks up a single [`McpServer`] by its registered name.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn get_mcp_server_by_name(
        &self,
        name: &str,
    ) -> Result<Option<McpServer>, IngotError> {
        let name = name.to_owned();
        self.run_blocking(move |ig| ig.get_mcp_server_by_name(&name))
            .await
    }

    /// Finds the MCP server that exposes a tool with the given `tool_name`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] if the tool-list query fails, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn find_mcp_server_for_tool(
        &self,
        tool_name: &str,
    ) -> Result<Option<McpServer>, IngotError> {
        let tool_name = tool_name.to_owned();
        self.run_blocking(move |ig| ig.find_mcp_server_for_tool(&tool_name))
            .await
    }

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

    // ── metrics_rollups ───────────────────────────────────────────────────────

    /// Computes time-tiered metrics buckets for `tier` over `[since, until)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying queries, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn metrics_rollup(
        &self,
        tier: crate::RollupTier,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<crate::MetricsBucket>, IngotError> {
        self.run_blocking(move |ig| ig.metrics_rollup(tier, since, until))
            .await
    }

    /// Upserts the computed buckets for `tier` over `[epoch, until)` into the
    /// `metrics_rollups` cache. Idempotent.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying queries or upsert, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn materialise_rollups(
        &self,
        tier: crate::RollupTier,
        until: Timestamp,
    ) -> Result<Vec<crate::MetricsBucket>, IngotError> {
        self.run_blocking(move |ig| ig.materialise_rollups(tier, until))
            .await
    }

    // ── savings_rollup ────────────────────────────────────────────────────────

    /// Computes time-tiered savings buckets for `tier` over `[since, until)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn savings_rollup(
        &self,
        tier: crate::RollupTier,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<Vec<crate::SavingsBucket>, IngotError> {
        self.run_blocking(move |ig| ig.savings_rollup(tier, since, until))
            .await
    }

    /// Computes the efficiency ratio `saved / (saved + billed_input)` for `tier`
    /// over `[since, until)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying queries, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn efficiency_ratio(
        &self,
        tier: crate::RollupTier,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<f64, IngotError> {
        self.run_blocking(move |ig| ig.efficiency_ratio(tier, since, until))
            .await
    }

    /// Computes the full [`crate::SavingsSummary`] for `tier` over `[since, until)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying queries, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn savings_summary(
        &self,
        tier: crate::RollupTier,
        since: Timestamp,
        until: Timestamp,
    ) -> Result<crate::SavingsSummary, IngotError> {
        self.run_blocking(move |ig| ig.savings_summary(tier, since, until))
            .await
    }

    // ── token_snapshots ───────────────────────────────────────────────────────

    /// Saves a [`TokenSnapshot`].
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying upsert, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn save_token_snapshot(&self, snap: TokenSnapshot) -> Result<(), IngotError> {
        self.run_blocking(move |ig| ig.save_token_snapshot(&snap))
            .await
    }

    /// Returns all [`TokenSnapshot`]s for `session_id`, ordered by `turn_n` ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn session_token_snapshots(
        &self,
        session_id: &str,
    ) -> Result<Vec<TokenSnapshot>, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.session_token_snapshots(&session_id))
            .await
    }

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

    // ── prompt_hashes ─────────────────────────────────────────────────────────

    /// Records a prompt content hash for `(change, role)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn save_prompt_hash(
        &self,
        change: &str,
        role: &str,
        hash: &str,
    ) -> Result<(), IngotError> {
        let change = change.to_owned();
        let role = role.to_owned();
        let hash = hash.to_owned();
        self.run_blocking(move |ig| ig.save_prompt_hash(&change, &role, &hash))
            .await
    }

    /// Returns the most recent prompt hash for `(change, role)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn get_prompt_hash(
        &self,
        change: &str,
        role: &str,
    ) -> Result<Option<String>, IngotError> {
        let change = change.to_owned();
        let role = role.to_owned();
        self.run_blocking(move |ig| ig.get_prompt_hash(&change, &role))
            .await
    }

    /// Returns all prompt hash records for `change`, ordered by `ts` ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_prompt_hashes(
        &self,
        change: &str,
    ) -> Result<Vec<PromptHashRecord>, IngotError> {
        let change = change.to_owned();
        self.run_blocking(move |ig| ig.list_prompt_hashes(&change))
            .await
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;
    use uuid::Uuid;

    fn make_handle() -> IngotHandle {
        let ingot = Ingot::open_in_memory().expect("in-memory db failed");
        IngotHandle::new(ingot)
    }

    fn sample_session() -> Session {
        Session {
            id: Uuid::new_v4(),
            created_at: Timestamp::from_secs_f64(1_700_000_000.0),
            updated_at: Timestamp::from_secs_f64(1_700_000_000.0),
            status: "active".to_owned(),
            task_id: None,
            mode: None,
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        }
    }

    #[tokio::test]
    async fn ingot_handle_get_session_returns_none_for_unknown_id() {
        let handle = make_handle();
        let result = handle.get_session("nonexistent-id").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn ingot_handle_save_and_load_session_roundtrip() {
        let handle = make_handle();
        let session = sample_session();
        let id = session.id.to_string();

        handle.create_session(session.clone()).await.unwrap();

        let fetched = handle.get_session(&id).await.unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.id, session.id);
        assert_eq!(fetched.status, "active");
    }

    #[tokio::test]
    async fn run_blocking_panic_surfaces_task_panic() {
        let handle = make_handle();
        let result: Result<(), IngotError> = handle
            .run_blocking(|_ig| panic!("boom inside blocking closure"))
            .await;
        match result {
            Err(IngotError::TaskPanic(_)) => {}
            other => panic!("expected TaskPanic, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poisoned_lock_recovers_and_subsequent_calls_succeed() {
        let handle = make_handle();

        // Poison the mutex by panicking while the lock is held.
        let panicked: Result<(), IngotError> =
            handle.run_blocking(|_ig| panic!("poison the lock")).await;
        assert!(matches!(panicked, Err(IngotError::TaskPanic(_))));

        // The connection remains valid: a subsequent operation must still work
        // because run_blocking recovers the poisoned guard via into_inner().
        let session = sample_session();
        let id = session.id.to_string();
        handle
            .create_session(session)
            .await
            .expect("operation after poison must succeed");
        let fetched = handle.get_session(&id).await.unwrap();
        assert!(fetched.is_some(), "row written after poison recovery");
    }
}
