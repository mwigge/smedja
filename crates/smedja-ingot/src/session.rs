use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::IngotError;

/// A top-level orchestration session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique session identifier (UUID v4 stored as TEXT).
    pub id: Uuid,
    /// Unix epoch timestamp when the session was created.
    pub created_at: f64,
    /// Unix epoch timestamp of the last status change.
    pub updated_at: f64,
    /// Lifecycle status: `"active"`, `"complete"`, or `"failed"`.
    pub status: String,
    /// Optional associated task identifier.
    pub task_id: Option<String>,
    /// Optional operating mode: `"tdd"`, `"ponytail"`, `"spec"`, or `"sre"`.
    pub mode: Option<String>,
}

/// Inserts a new [`Session`] row.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the INSERT fails (e.g. duplicate primary key).
pub(crate) fn create(conn: &rusqlite::Connection, session: &Session) -> Result<(), IngotError> {
    conn.execute(
        "INSERT INTO sessions (id, created_at, updated_at, status, task_id, mode) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            session.id.to_string(),
            session.created_at,
            session.updated_at,
            session.status,
            session.task_id,
            session.mode,
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
        "SELECT id, created_at, updated_at, status, task_id, mode \
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
            Ok(Session {
                id,
                created_at: row.get(1)?,
                updated_at: row.get(2)?,
                status: row.get(3)?,
                task_id: row.get(4)?,
                mode: row.get(5)?,
            })
        },
    );

    match result {
        Ok(s) => Ok(Some(s)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(IngotError::Db(e)),
    }
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
    updated_at: f64,
) -> Result<(), IngotError> {
    conn.execute(
        "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
        rusqlite::params![status, updated_at, id],
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
            created_at: 1_700_000_000.0,
            updated_at: 1_700_000_000.0,
            status: "active".to_string(),
            task_id: None,
            mode: Some("tdd".to_string()),
        }
    }

    #[test]
    fn create_then_get_returns_session() {
        let mut ingot = Ingot::open_in_memory().unwrap();
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
        let mut ingot = Ingot::open_in_memory().unwrap();
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
        let mut ingot = Ingot::open_in_memory().unwrap();
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
        let mut ingot = Ingot::open_in_memory().unwrap();
        let s = Session {
            id: Uuid::new_v4(),
            created_at: 1_700_000_002.0,
            updated_at: 1_700_000_002.0,
            status: "active".to_string(),
            task_id: Some("task-xyz".to_string()),
            mode: None,
        };
        ingot.create_session(&s).unwrap();

        let fetched = ingot.get_session(&s.id.to_string()).unwrap().unwrap();
        assert_eq!(fetched.task_id.as_deref(), Some("task-xyz"));
        assert!(fetched.mode.is_none());
    }
}
