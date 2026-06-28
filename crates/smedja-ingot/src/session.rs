use serde::{Deserialize, Serialize};
use smedja_types::Timestamp;
use uuid::Uuid;

use crate::error::IngotError;

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
}
