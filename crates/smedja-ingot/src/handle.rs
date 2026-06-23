//! Async facade over [`Ingot`].
//!
//! [`IngotHandle`] wraps a [`std::sync::Mutex`]-guarded [`Ingot`] in an
//! [`Arc`] so it is cheaply [`Clone`]-able across task boundaries. Every
//! method delegates to the corresponding [`Ingot`] method inside
//! [`tokio::task::spawn_blocking`], keeping `SQLite` I/O off the Tokio
//! executor thread-pool.

use std::sync::Arc;

use crate::{
    AuditEvent, Checkpoint, ConversationRollup, CostEntry, CostRow, Ingot, IngotError, LoopRecord,
    McpServer, PromptHashRecord, Session, Task, TokenSnapshot,
};

/// Converts a [`tokio::task::JoinError`] (i.e. a panic inside `spawn_blocking`)
/// into an [`IngotError`] so callers see a uniform error type.
#[allow(clippy::needless_pass_by_value)] // used as `.map_err(join_err)`, which requires taking the error by value
fn join_err(e: tokio::task::JoinError) -> IngotError {
    IngotError::Db(rusqlite::Error::InvalidParameterName(format!(
        "spawn_blocking panic: {e}"
    )))
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

    // ── audit_events ────────────────────────────────────────────────────────

    /// Appends an [`AuditEvent`] to the audit log.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT.
    pub async fn insert_audit_event(&self, event: AuditEvent) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .insert_audit_event(&event)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns all [`AuditEvent`]s for `session_id`, ordered by `ts` ascending.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn list_audit_events(&self, session_id: &str) -> Result<Vec<AuditEvent>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .list_audit_events(&session_id)
        })
        .await
        .map_err(join_err)?
    }

    /// Persists a timeline event and upserts the matching [`ConversationRollup`].
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from either the INSERT or the upsert.
    pub async fn record_timeline_event(&self, event: AuditEvent) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .record_timeline_event(&event)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns the most recent `limit` [`ConversationRollup`]s by `last_seen_at` descending.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn recent_conversations(
        &self,
        limit: u32,
    ) -> Result<Vec<ConversationRollup>, IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .recent_conversations(limit)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns timeline events for `conversation_id`, ordered by `rowid` ascending.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn conversation_timeline(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<AuditEvent>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let conversation_id = conversation_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .conversation_timeline(&conversation_id)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns timeline events with `status = 'error'` for `conversation_id`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn failed_events(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<AuditEvent>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let conversation_id = conversation_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .failed_events(&conversation_id)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns all [`AuditEvent`]s, ordered by `ts` ascending.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn list_all_audit_events(&self) -> Result<Vec<AuditEvent>, IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .list_all_audit_events()
        })
        .await
        .map_err(join_err)?
    }

    // ── sessions ─────────────────────────────────────────────────────────────

    /// Inserts a new [`Session`].
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT.
    pub async fn create_session(&self, session: Session) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .create_session(&session)
        })
        .await
        .map_err(join_err)?
    }

    /// Retrieves a [`Session`] by `id`, returning `None` when not found.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn get_session(&self, id: &str) -> Result<Option<Session>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner.lock().expect("ingot mutex poisoned").get_session(&id)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns all [`Session`]s ordered by `created_at` ascending.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn list_sessions(&self) -> Result<Vec<Session>, IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner.lock().expect("ingot mutex poisoned").list_sessions()
        })
        .await
        .map_err(join_err)?
    }

    /// Deletes the session with the given `id`.
    ///
    /// Returns `true` if a row was deleted, `false` if none existed.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying DELETE.
    pub async fn delete_session(&self, id: &str) -> Result<bool, IngotError> {
        let inner = Arc::clone(&self.inner);
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .delete_session(&id)
        })
        .await
        .map_err(join_err)?
    }

    /// Updates the `status` of a session.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE.
    pub async fn update_session_status(&self, id: &str, status: &str) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let id = id.to_owned();
        let status = status.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .update_session_status(&id, &status)
        })
        .await
        .map_err(join_err)?
    }

    /// Sets the `cowork_mode` flag for the session identified by `session_id`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE.
    pub async fn set_cowork_mode(&self, session_id: &str, enabled: bool) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .set_cowork_mode(&session_id, enabled)
        })
        .await
        .map_err(join_err)?
    }

    /// Sets the `workspace_root` path for a session.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE.
    pub async fn update_session_workspace_root(
        &self,
        session_id: &str,
        workspace_root: &str,
    ) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        let workspace_root = workspace_root.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .update_session_workspace_root(&session_id, &workspace_root)
        })
        .await
        .map_err(join_err)?
    }

    /// Sets the `mode` field for a session.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE.
    pub async fn update_session_mode(
        &self,
        session_id: &str,
        mode: &str,
    ) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        let mode = mode.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .update_session_mode(&session_id, &mode)
        })
        .await
        .map_err(join_err)?
    }

    /// Sets the `model_override` for a session.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE.
    pub async fn update_session_model_override(
        &self,
        session_id: &str,
        model: &str,
    ) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        let model = model.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .update_session_model_override(&session_id, &model)
        })
        .await
        .map_err(join_err)?
    }

    /// Sets the `runner_override` for a session.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE.
    pub async fn update_session_runner_override(
        &self,
        session_id: &str,
        runner: &str,
    ) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        let runner = runner.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .update_session_runner_override(&session_id, &runner)
        })
        .await
        .map_err(join_err)?
    }

    /// Links a session to a task.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE.
    pub async fn update_session_task_id(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        let task_id = task_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .update_session_task_id(&session_id, &task_id)
        })
        .await
        .map_err(join_err)?
    }

    /// Enables or disables the cowork gate for a session.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE.
    pub async fn update_session_cowork_mode(
        &self,
        session_id: &str,
        enabled: bool,
    ) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .update_session_cowork_mode(&session_id, enabled)
        })
        .await
        .map_err(join_err)?
    }

    // ── mcp_servers ──────────────────────────────────────────────────────────

    /// Registers (or replaces) an [`McpServer`].
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT OR REPLACE.
    pub async fn register_mcp_server(&self, server: McpServer) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .register_mcp_server(&server)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns all registered [`McpServer`]s ordered by `name` ascending.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn list_mcp_servers(&self) -> Result<Vec<McpServer>, IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .list_mcp_servers()
        })
        .await
        .map_err(join_err)?
    }

    /// Removes the [`McpServer`] with the given `id`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying DELETE.
    pub async fn remove_mcp_server(&self, id: &str) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .remove_mcp_server(&id)
        })
        .await
        .map_err(join_err)?
    }

    /// Updates the cached tool list for a server.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE.
    pub async fn update_mcp_tools(&self, name: &str, tools_json: &str) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_owned();
        let tools_json = tools_json.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .update_mcp_tools(&name, &tools_json)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns stale [`McpServer`]s whose `last_refresh` is older than `older_than_secs`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn get_stale_servers(
        &self,
        older_than_secs: f64,
    ) -> Result<Vec<McpServer>, IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .get_stale_servers(older_than_secs)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns all `(server_name, tools_json)` pairs for servers with non-empty tool lists.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn get_all_mcp_tools(&self) -> Result<Vec<(String, String)>, IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .get_all_mcp_tools()
        })
        .await
        .map_err(join_err)?
    }

    /// Looks up a single [`McpServer`] by its registered name.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn get_mcp_server_by_name(
        &self,
        name: &str,
    ) -> Result<Option<McpServer>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .get_mcp_server_by_name(&name)
        })
        .await
        .map_err(join_err)?
    }

    /// Finds the MCP server that exposes a tool with the given `tool_name`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] if the tool-list query fails.
    pub async fn find_mcp_server_for_tool(
        &self,
        tool_name: &str,
    ) -> Result<Option<McpServer>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let tool_name = tool_name.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .find_mcp_server_for_tool(&tool_name)
        })
        .await
        .map_err(join_err)?
    }

    // ── tasks ─────────────────────────────────────────────────────────────────

    /// Inserts a new [`Task`].
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT.
    pub async fn create_task(&self, task: Task) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .create_task(&task)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns tasks, optionally filtered by `status`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn list_tasks(&self, status: Option<String>) -> Result<Vec<Task>, IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .list_tasks(status.as_deref())
        })
        .await
        .map_err(join_err)?
    }

    /// Updates the `status` field for a task.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE.
    pub async fn update_task_status(&self, id: &str, status: &str) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let id = id.to_owned();
        let status = status.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .update_task_status(&id, &status)
        })
        .await
        .map_err(join_err)?
    }

    /// Retrieves a [`Task`] by `id`, returning `None` when not found.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn get_task(&self, id: &str) -> Result<Option<Task>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner.lock().expect("ingot mutex poisoned").get_task(&id)
        })
        .await
        .map_err(join_err)?
    }

    /// Stores `response` text for a task and sets `status = "complete"`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE.
    pub async fn set_task_response(&self, id: &str, response: &str) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let id = id.to_owned();
        let response = response.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .set_task_response(&id, &response)
        })
        .await
        .map_err(join_err)?
    }

    // ── checkpoints ───────────────────────────────────────────────────────────

    /// Saves a [`Checkpoint`], replacing any existing one for the same
    /// `(session_id, turn_n)` pair.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying upsert.
    pub async fn save_checkpoint(&self, cp: Checkpoint) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .save_checkpoint(&cp)
        })
        .await
        .map_err(join_err)?
    }

    /// Loads the [`Checkpoint`] for `(session_id, turn_n)`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn load_checkpoint(
        &self,
        session_id: &str,
        turn_n: u32,
    ) -> Result<Option<Checkpoint>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .load_checkpoint(&session_id, turn_n)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns the checkpoint with the highest `turn_n` for `session_id`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn latest_checkpoint(
        &self,
        session_id: &str,
    ) -> Result<Option<Checkpoint>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .latest_checkpoint(&session_id)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns all checkpoints for `session_id`, ordered by turn number ascending.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn list_checkpoints(&self, session_id: &str) -> Result<Vec<Checkpoint>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .list_checkpoints(&session_id)
        })
        .await
        .map_err(join_err)?
    }

    /// Atomically rolls back a session to `turn_n`, pruning later checkpoints.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from any SQL operation.
    pub async fn rollback_session(
        &self,
        session_id: &str,
        turn_n: u32,
    ) -> Result<Option<Checkpoint>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .rollback_session(&session_id, turn_n)
        })
        .await
        .map_err(join_err)?
    }

    // ── cost_ledger ───────────────────────────────────────────────────────────

    /// Appends a [`CostEntry`] to the cost ledger.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT.
    pub async fn insert_cost(&self, entry: CostEntry) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .insert_cost(&entry)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns the total `cost_usd` for all entries in `session_id`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn session_cost(&self, session_id: &str) -> Result<f64, IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .session_cost(&session_id)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns per-model/runner aggregate cost rows for `session_id`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn session_cost_entries(&self, session_id: &str) -> Result<Vec<CostRow>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .session_cost_entries(&session_id)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns the model name from the most recent cost entry for `session_id`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn session_last_model(&self, session_id: &str) -> Result<Option<String>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .session_last_model(&session_id)
        })
        .await
        .map_err(join_err)?
    }

    // ── token_snapshots ───────────────────────────────────────────────────────

    /// Saves a [`TokenSnapshot`].
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying upsert.
    pub async fn save_token_snapshot(&self, snap: TokenSnapshot) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .save_token_snapshot(&snap)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns all [`TokenSnapshot`]s for `session_id`, ordered by `turn_n` ascending.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn session_token_snapshots(
        &self,
        session_id: &str,
    ) -> Result<Vec<TokenSnapshot>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .session_token_snapshots(&session_id)
        })
        .await
        .map_err(join_err)?
    }

    // ── loops ─────────────────────────────────────────────────────────────────

    /// Inserts a new [`LoopRecord`].
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT.
    pub async fn create_loop(&self, rec: LoopRecord) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .create_loop(&rec)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns the spec-first methodology state for `session_id`, or the
    /// all-false default when no row exists.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn get_methodology_state(
        &self,
        session_id: &str,
    ) -> Result<crate::MethodologyState, IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .get_methodology_state(&session_id)
        })
        .await
        .map_err(join_err)?
    }

    /// Sets the `spec_recorded` flag for `session_id`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPSERT.
    pub async fn set_spec_recorded(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .set_spec_recorded(&session_id, value)
        })
        .await
        .map_err(join_err)?
    }

    /// Sets the `approval_recorded` flag for `session_id`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPSERT.
    pub async fn set_approval_recorded(
        &self,
        session_id: &str,
        value: bool,
    ) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .set_approval_recorded(&session_id, value)
        })
        .await
        .map_err(join_err)?
    }

    /// Sets the per-session `no_spec_gate` escape-hatch flag for `session_id`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPSERT.
    pub async fn set_no_spec_gate(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .set_no_spec_gate(&session_id, value)
        })
        .await
        .map_err(join_err)?
    }

    /// Retrieves a [`LoopRecord`] by `id`, returning `None` when not found.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn get_loop(&self, id: &str) -> Result<Option<LoopRecord>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner.lock().expect("ingot mutex poisoned").get_loop(&id)
        })
        .await
        .map_err(join_err)?
    }

    /// Updates `status` and `updated_at` for a loop.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE.
    pub async fn update_loop_status(
        &self,
        id: &str,
        status: &str,
        updated_at: f64,
    ) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let id = id.to_owned();
        let status = status.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .update_loop_status(&id, &status, updated_at)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns all [`LoopRecord`]s for `change_name`, ordered by `created_at` descending.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn list_loops(&self, change_name: &str) -> Result<Vec<LoopRecord>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let change_name = change_name.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .list_loops(&change_name)
        })
        .await
        .map_err(join_err)?
    }

    /// Updates `current_slice` and `updated_at` for a loop.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPDATE.
    pub async fn update_loop_slice(
        &self,
        id: &str,
        current_slice: i64,
        updated_at: f64,
    ) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .update_loop_slice(&id, current_slice, updated_at)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns all [`LoopRecord`]s, optionally filtered by `status`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn list_loops_by_status(
        &self,
        status: Option<String>,
    ) -> Result<Vec<LoopRecord>, IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .list_loops_by_status(status.as_deref())
        })
        .await
        .map_err(join_err)?
    }

    // ── prompt_hashes ─────────────────────────────────────────────────────────

    /// Records a prompt content hash for `(change, role)`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT.
    pub async fn save_prompt_hash(
        &self,
        change: &str,
        role: &str,
        hash: &str,
    ) -> Result<(), IngotError> {
        let inner = Arc::clone(&self.inner);
        let change = change.to_owned();
        let role = role.to_owned();
        let hash = hash.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .save_prompt_hash(&change, &role, &hash)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns the most recent prompt hash for `(change, role)`.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn get_prompt_hash(
        &self,
        change: &str,
        role: &str,
    ) -> Result<Option<String>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let change = change.to_owned();
        let role = role.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .get_prompt_hash(&change, &role)
        })
        .await
        .map_err(join_err)?
    }

    /// Returns all prompt hash records for `change`, ordered by `ts` ascending.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query.
    pub async fn list_prompt_hashes(
        &self,
        change: &str,
    ) -> Result<Vec<PromptHashRecord>, IngotError> {
        let inner = Arc::clone(&self.inner);
        let change = change.to_owned();
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .list_prompt_hashes(&change)
        })
        .await
        .map_err(join_err)?
    }

    // ── JSONL export / import ─────────────────────────────────────────────────

    /// Exports tasks and their associated audit events as a JSONL stream.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] or [`IngotError::Json`] from the underlying
    /// export logic.
    pub async fn export_jsonl(
        &self,
        change: Option<String>,
    ) -> Result<Vec<serde_json::Value>, IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .export_jsonl(change.as_deref())
        })
        .await
        .map_err(join_err)?
    }

    /// Imports tasks and audit events from a JSONL stream.
    ///
    /// # Panics
    ///
    /// Panics if the ingot mutex is poisoned (a prior holder panicked while
    /// holding the lock).
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Json`] or [`IngotError::Db`] from the underlying
    /// import logic.
    pub async fn import_jsonl(&self, records: Vec<serde_json::Value>) -> Result<usize, IngotError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .lock()
                .expect("ingot mutex poisoned")
                .import_jsonl(&records)
        })
        .await
        .map_err(join_err)?
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
            created_at: 1_700_000_000.0,
            updated_at: 1_700_000_000.0,
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
}
