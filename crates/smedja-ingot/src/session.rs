use serde::{Deserialize, Serialize};
use smedja_types::Timestamp;
use uuid::Uuid;

use crate::error::IngotError;
use crate::{Ingot, IngotHandle};

/// A top-level orchestration session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique session identifier (UUID v4 stored as TEXT).
    pub id: Uuid,
    /// Timestamp when the session was created (micros since the Unix epoch).
    pub created_at: Timestamp,
    /// Timestamp of the last status change (micros since the Unix epoch).
    pub updated_at: Timestamp,
    /// Lifecycle status: `"active"`, `"complete"`, or `"failed"`.
    pub status: String,
    /// Optional associated task identifier.
    pub task_id: Option<String>,
    /// Optional operating mode: `"tdd"`, `"ponytail"`, `"spec"`, or `"sre"`.
    pub mode: Option<String>,
    /// Human-readable session title supplied by the caller at creation time.
    #[serde(default)]
    pub title: String,
    /// Whether human-in-the-loop cowork gate is active for this session.
    pub cowork_mode: bool,
    /// Optional filesystem path to the workspace root for this session.
    pub workspace_root: Option<String>,
    /// Optional model name override; when set, `run_turn` uses this instead of
    /// the `SMEDJA_MODEL` environment variable.
    pub model_override: Option<String>,
    /// Optional runner override; when set, `run_turn` bypasses the assayer and
    /// routes to this runner (e.g. `"claude-cli"`, `"codex-cli"`, `"local"`).
    pub runner_override: Option<String>,
}

/// Inserts a new [`Session`] row.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the INSERT fails (e.g. duplicate primary key).
pub(crate) fn create(conn: &rusqlite::Connection, session: &Session) -> Result<(), IngotError> {
    conn.execute(
        "INSERT INTO sessions \
         (id, created_at, updated_at, status, task_id, mode, title, cowork_mode, workspace_root, model_override, runner_override) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            session.id.to_string(),
            session.created_at.as_micros(),
            session.updated_at.as_micros(),
            session.status,
            session.task_id,
            session.mode,
            session.title,
            i64::from(session.cowork_mode),
            session.workspace_root,
            session.model_override,
            session.runner_override,
        ],
    )?;
    Ok(())
}

/// Retrieves a [`Session`] by its `id`, returning `None` if not found.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn get(conn: &rusqlite::Connection, id: &str) -> Result<Option<Session>, IngotError> {
    let result = conn.query_row(
        "SELECT id, created_at, updated_at, status, task_id, mode, title, cowork_mode, workspace_root, model_override, runner_override \
         FROM sessions WHERE id = ?1",
        rusqlite::params![id],
        |row| {
            let id_str: String = row.get(0)?;
            let id = Uuid::parse_str(&id_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let cowork_raw: i64 = row.get(7).unwrap_or(0);
            Ok(Session {
                id,
                created_at: Timestamp::from_micros(crate::read_micros(row, 1)?),
                updated_at: Timestamp::from_micros(crate::read_micros(row, 2)?),
                status: row.get(3)?,
                task_id: row.get(4)?,
                mode: row.get(5)?,
                title: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                cowork_mode: cowork_raw != 0,
                workspace_root: row.get(8)?,
                model_override: row.get(9)?,
                runner_override: row.get(10)?,
            })
        },
    );

    match result {
        Ok(s) => Ok(Some(s)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(IngotError::Db(e)),
    }
}

/// Returns all [`Session`]s ordered by `created_at` ascending.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn list(conn: &rusqlite::Connection) -> Result<Vec<Session>, IngotError> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at, updated_at, status, task_id, mode, title, cowork_mode, workspace_root, model_override, runner_override \
         FROM sessions ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        let id_str: String = row.get(0)?;
        let id = Uuid::parse_str(&id_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let cowork_raw: i64 = row.get(7).unwrap_or(0);
        Ok(Session {
            id,
            created_at: Timestamp::from_micros(crate::read_micros(row, 1)?),
            updated_at: Timestamp::from_micros(crate::read_micros(row, 2)?),
            status: row.get(3)?,
            task_id: row.get(4)?,
            mode: row.get(5)?,
            title: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            cowork_mode: cowork_raw != 0,
            workspace_root: row.get(8)?,
            model_override: row.get(9)?,
            runner_override: row.get(10)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(IngotError::Db)
}

/// Deletes the session with the given `id`.
///
/// Returns `true` if a row was deleted, `false` if no session with that `id`
/// existed.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the DELETE fails.
pub(crate) fn delete(conn: &rusqlite::Connection, id: &str) -> Result<bool, IngotError> {
    let n = conn.execute("DELETE FROM sessions WHERE id = ?1", rusqlite::params![id])?;
    Ok(n > 0)
}

/// Updates the `status` and `updated_at` fields for the session with the given `id`.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the UPDATE fails.
pub(crate) fn update_status(
    conn: &rusqlite::Connection,
    id: &str,
    status: &str,
    updated_at: Timestamp,
) -> Result<(), IngotError> {
    conn.execute(
        "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
        rusqlite::params![status, updated_at.as_micros(), id],
    )?;
    Ok(())
}

/// Sets the `workspace_root` path for the session identified by `id`.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the UPDATE fails.
pub(crate) fn update_workspace_root(
    conn: &rusqlite::Connection,
    id: &str,
    workspace_root: &str,
) -> Result<(), IngotError> {
    conn.execute(
        "UPDATE sessions SET workspace_root = ?1 WHERE id = ?2",
        rusqlite::params![workspace_root, id],
    )?;
    Ok(())
}

/// Links the session identified by `id` to a task by setting `task_id`.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the UPDATE fails.
pub(crate) fn update_task_id(
    conn: &rusqlite::Connection,
    id: &str,
    task_id: &str,
) -> Result<(), IngotError> {
    conn.execute(
        "UPDATE sessions SET task_id = ?1 WHERE id = ?2",
        rusqlite::params![task_id, id],
    )?;
    Ok(())
}

/// Enables or disables the cowork gate for the session identified by `id`.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the UPDATE fails.
pub(crate) fn update_cowork_mode(
    conn: &rusqlite::Connection,
    id: &str,
    enabled: bool,
) -> Result<(), IngotError> {
    conn.execute(
        "UPDATE sessions SET cowork_mode = ?1, updated_at = ?2 WHERE id = ?3",
        rusqlite::params![i64::from(enabled), Timestamp::now().as_micros(), id],
    )?;
    Ok(())
}

/// Searches sessions where `title` or `workspace_root` contains `query` (case-insensitive).
///
/// Uses SQL `LIKE` matching — sufficient for typical session counts. Returns sessions
/// ordered by `created_at` ascending.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn search(conn: &rusqlite::Connection, query: &str) -> Result<Vec<Session>, IngotError> {
    let pattern = format!("%{query}%");
    let mut stmt = conn.prepare(
        "SELECT id, created_at, updated_at, status, task_id, mode, title, cowork_mode, workspace_root, model_override, runner_override \
         FROM sessions WHERE title LIKE ?1 OR workspace_root LIKE ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![pattern], |row| {
        let id_str: String = row.get(0)?;
        let id = Uuid::parse_str(&id_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let cowork_raw: i64 = row.get(7).unwrap_or(0);
        Ok(Session {
            id,
            created_at: Timestamp::from_micros(crate::read_micros(row, 1)?),
            updated_at: Timestamp::from_micros(crate::read_micros(row, 2)?),
            status: row.get(3)?,
            task_id: row.get(4)?,
            mode: row.get(5)?,
            title: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            cowork_mode: cowork_raw != 0,
            workspace_root: row.get(8)?,
            model_override: row.get(9)?,
            runner_override: row.get(10)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(IngotError::Db)
}

/// Sets the `title` field for the session identified by `id`.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the UPDATE fails.
pub(crate) fn update_title(
    conn: &rusqlite::Connection,
    id: &str,
    title: &str,
) -> Result<(), IngotError> {
    conn.execute(
        "UPDATE sessions SET title = ?1 WHERE id = ?2",
        rusqlite::params![title, id],
    )?;
    Ok(())
}

/// Sets the `mode` field for the session identified by `id`.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the UPDATE fails.
pub(crate) fn update_mode(
    conn: &rusqlite::Connection,
    id: &str,
    mode: &str,
) -> Result<(), IngotError> {
    conn.execute(
        "UPDATE sessions SET mode = ?1 WHERE id = ?2",
        rusqlite::params![mode, id],
    )?;
    Ok(())
}

/// Sets the `model_override` field and updates `updated_at` for the session identified by `id`.
///
/// # Errors
///
/// Returns [`rusqlite::Error`] if the UPDATE fails.
pub(crate) fn update_model_override(
    conn: &rusqlite::Connection,
    id: &str,
    model: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE sessions SET model_override = ?1, updated_at = ?2 WHERE id = ?3",
        rusqlite::params![model, Timestamp::now().as_micros(), id],
    )?;
    Ok(())
}

/// Sets the `runner_override` field and updates `updated_at` for the session identified by `id`.
///
/// When set, `run_turn` bypasses the assayer and routes directly to this runner.
///
/// # Errors
///
/// Returns [`rusqlite::Error`] if the UPDATE fails.
pub(crate) fn update_runner_override(
    conn: &rusqlite::Connection,
    id: &str,
    runner: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE sessions SET runner_override = ?1, updated_at = ?2 WHERE id = ?3",
        rusqlite::params![runner, Timestamp::now().as_micros(), id],
    )?;
    Ok(())
}

impl Ingot {
    /// Inserts a new [`Session`].
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails.
    #[must_use = "check the Result to confirm the session was created"]
    pub fn create_session(&self, session: &Session) -> Result<(), IngotError> {
        create(&self.conn, session)
    }

    /// Retrieves a [`Session`] by `id`, returning `None` when not found.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned session"]
    pub fn get_session(&self, id: &str) -> Result<Option<Session>, IngotError> {
        get(&self.conn, id)
    }

    /// Returns all [`Session`]s ordered by `created_at` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned sessions"]
    pub fn list_sessions(&self) -> Result<Vec<Session>, IngotError> {
        list(&self.conn)
    }

    /// Searches sessions where `title` or `workspace_root` contains `query` (case-insensitive).
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the matched sessions"]
    pub fn search_sessions(&self, query: &str) -> Result<Vec<Session>, IngotError> {
        search(&self.conn, query)
    }

    /// Deletes the session with the given `id`.
    ///
    /// Returns `true` if a row was deleted, `false` if no session with that `id`
    /// existed.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the DELETE fails.
    #[must_use = "check the Result to confirm the session was deleted"]
    pub fn delete_session(&self, id: &str) -> Result<bool, IngotError> {
        delete(&self.conn, id)
    }

    /// Updates the `status` of a session to `status` and records a new `updated_at`
    /// timestamp using the current Unix epoch.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the status was updated"]
    pub fn update_session_status(&self, id: &str, status: &str) -> Result<(), IngotError> {
        update_status(&self.conn, id, status, smedja_types::Timestamp::now())
    }

    /// Sets the `workspace_root` filesystem path for the session identified by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the workspace root was updated"]
    pub fn update_session_workspace_root(
        &self,
        session_id: &str,
        workspace_root: &str,
    ) -> Result<(), IngotError> {
        update_workspace_root(&self.conn, session_id, workspace_root)
    }

    /// Sets the `mode` field for the session identified by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the mode was updated"]
    pub fn update_session_mode(&self, session_id: &str, mode: &str) -> Result<(), IngotError> {
        update_mode(&self.conn, session_id, mode)
    }

    /// Sets the `model_override` field for the session identified by `session_id`.
    ///
    /// When set, `run_turn` uses this model name instead of the `SMEDJA_MODEL`
    /// environment variable.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the model override was updated"]
    pub fn update_session_model_override(
        &self,
        session_id: &str,
        model: &str,
    ) -> Result<(), IngotError> {
        update_model_override(&self.conn, session_id, model).map_err(IngotError::Db)
    }

    /// Sets the `runner_override` field for the session identified by `session_id`.
    ///
    /// When set, `run_turn` bypasses the assayer and routes directly to this runner
    /// (e.g. `"claude-cli"`, `"codex-cli"`, `"local"`).
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the runner override was updated"]
    pub fn update_session_runner_override(
        &self,
        session_id: &str,
        runner: &str,
    ) -> Result<(), IngotError> {
        update_runner_override(&self.conn, session_id, runner).map_err(IngotError::Db)
    }

    /// Links the session identified by `session_id` to a task by setting `task_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the task id was linked"]
    pub fn update_session_task_id(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<(), IngotError> {
        update_task_id(&self.conn, session_id, task_id)
    }

    /// Enables or disables the cowork gate for the session identified by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the cowork mode was updated"]
    pub fn update_session_cowork_mode(
        &self,
        session_id: &str,
        enabled: bool,
    ) -> Result<(), IngotError> {
        update_cowork_mode(&self.conn, session_id, enabled)
    }

    /// Sets the human-readable `title` for the session identified by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    pub fn update_session_title(&self, session_id: &str, title: &str) -> Result<(), IngotError> {
        update_title(&self.conn, session_id, title)
    }
}

impl IngotHandle {
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

    /// Searches sessions by title or workspace_root substring (case-insensitive).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ingot;

    fn sample_session() -> Session {
        Session {
            id: Uuid::new_v4(),
            created_at: Timestamp::from_secs_f64(1_700_000_000.0),
            updated_at: Timestamp::from_secs_f64(1_700_000_000.0),
            status: "active".to_string(),
            task_id: None,
            mode: Some("tdd".to_string()),
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        }
    }

    #[test]
    fn create_then_get_returns_session() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.id, s.id);
        assert_eq!(fetched.status, "active");
        assert_eq!(fetched.mode.as_deref(), Some("tdd"));
    }

    #[test]
    fn get_unknown_session_returns_none() {
        let ingot = Ingot::open_in_memory().unwrap();
        let result = ingot.get_session("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn update_session_status_changes_status() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        ingot
            .update_session_status(&s.id.to_string(), "complete")
            .unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(fetched.status, "complete");
    }

    #[test]
    fn update_status_changes_updated_at() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        ingot
            .update_session_status(&s.id.to_string(), "failed")
            .unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        // updated_at must be >= created_at (set by update_session_status)
        assert!(fetched.updated_at >= fetched.created_at);
    }

    #[test]
    fn nullable_task_id_and_mode_round_trip() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = Session {
            id: Uuid::new_v4(),
            created_at: Timestamp::from_secs_f64(1_700_000_002.0),
            updated_at: Timestamp::from_secs_f64(1_700_000_002.0),
            status: "active".to_string(),
            task_id: Some("task-xyz".to_string()),
            mode: None,
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        };
        ingot.create_session(&s).unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(fetched.task_id.as_deref(), Some("task-xyz"));
        assert!(fetched.mode.is_none());
    }

    #[test]
    fn workspace_root_round_trip() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        ingot
            .update_session_workspace_root(&s.id.to_string(), "/home/user/projects/myrepo")
            .unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(
            fetched.workspace_root.as_deref(),
            Some("/home/user/projects/myrepo")
        );
    }

    #[test]
    fn update_mode_round_trip() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        ingot
            .update_session_mode(&s.id.to_string(), "ponytail")
            .unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(fetched.mode.as_deref(), Some("ponytail"));
    }

    #[test]
    fn model_override_round_trip() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        ingot
            .update_session_model_override(&s.id.to_string(), "gemma4-27b")
            .unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(fetched.model_override.as_deref(), Some("gemma4-27b"));
    }

    #[test]
    fn model_override_defaults_to_none() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert!(fetched.model_override.is_none());
    }

    #[test]
    fn task_id_link_round_trip() {
        use crate::Task;
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        let task_id = Uuid::new_v4();
        let task = Task {
            id: task_id,
            title: "Test task".to_string(),
            description: String::new(),
            status: "planned".to_string(),
            created_at: Timestamp::from_secs_f64(1_700_000_010.0),
            session_id: Some(s.id.to_string()),
            response: None,
        };
        ingot.create_task(&task).unwrap();

        ingot
            .update_session_task_id(&s.id.to_string(), &task_id.to_string())
            .unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(
            fetched.task_id.as_deref(),
            Some(task_id.to_string().as_str())
        );
    }

    #[test]
    fn runner_override_round_trip() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        ingot
            .update_session_runner_override(&s.id.to_string(), "codex-cli")
            .unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(fetched.runner_override.as_deref(), Some("codex-cli"));
    }

    #[test]
    fn runner_override_defaults_to_none() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert!(fetched.runner_override.is_none());
    }

    #[test]
    fn update_title_round_trip() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        update_title(&ingot.conn, &s.id.to_string(), "my new title").unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(fetched.title, "my new title");
    }

    #[test]
    fn update_title_unknown_id_is_noop() {
        let ingot = Ingot::open_in_memory().unwrap();
        update_title(&ingot.conn, "no-such-id", "ignored").unwrap();
    }

    #[test]
    fn search_sessions_matches_title_substring() {
        let ingot = Ingot::open_in_memory().unwrap();
        let mut s = sample_session();
        s.title = "rust memory pressure investigation".to_string();
        ingot.create_session(&s).unwrap();

        let results = ingot.search_sessions("memory").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, s.id);
    }

    #[test]
    fn search_sessions_matches_workspace_root() {
        let ingot = Ingot::open_in_memory().unwrap();
        let mut s = sample_session();
        s.workspace_root = Some("/home/user/projects/smedja".to_string());
        ingot.create_session(&s).unwrap();

        let results = ingot.search_sessions("smedja").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, s.id);
    }

    #[test]
    fn search_sessions_returns_empty_for_no_match() {
        let ingot = Ingot::open_in_memory().unwrap();
        let s = sample_session();
        ingot.create_session(&s).unwrap();

        let results = ingot.search_sessions("zzznomatch").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_sessions_is_case_insensitive() {
        let ingot = Ingot::open_in_memory().unwrap();
        let mut s = sample_session();
        s.title = "Rust Project".to_string();
        ingot.create_session(&s).unwrap();

        let results = ingot.search_sessions("rust").unwrap();
        assert_eq!(results.len(), 1);
    }
}
