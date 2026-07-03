use smedja_types::Timestamp;
use uuid::Uuid;

use crate::error::IngotError;

use super::Session;

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

/// Searches sessions where `title` or `workspace_root` contains `query` (case-insensitive).
///
/// Uses SQL `LIKE` matching — sufficient for typical session counts. Returns sessions
/// ordered by `created_at` ascending.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn search(conn: &rusqlite::Connection, query: &str) -> Result<Vec<Session>, IngotError> {
    // Escape LIKE metacharacters so a query containing % or _ matches literally.
    let escaped = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("%{escaped}%");
    let mut stmt = conn.prepare(
        "SELECT id, created_at, updated_at, status, task_id, mode, title, cowork_mode, workspace_root, model_override, runner_override \
         FROM sessions WHERE title LIKE ?1 ESCAPE '\\' OR workspace_root LIKE ?1 ESCAPE '\\' ORDER BY created_at ASC",
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
