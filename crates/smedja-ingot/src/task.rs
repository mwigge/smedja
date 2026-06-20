use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::IngotError;

/// A structured unit of work within a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique task identifier (UUID v4 stored as TEXT).
    pub id: Uuid,
    /// Short human-readable title.
    pub title: String,
    /// Longer description of what the task entails.
    pub description: String,
    /// Lifecycle status: `"planned"`, `"in_progress"`, `"complete"`, or `"failed"`.
    pub status: String,
    /// Unix epoch timestamp when the task was created.
    pub created_at: f64,
    /// Optional session that owns this task.
    pub session_id: Option<String>,
}

/// Inserts a new [`Task`] row.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the INSERT fails (e.g. duplicate primary key).
pub(crate) fn create(conn: &rusqlite::Connection, task: &Task) -> Result<(), IngotError> {
    conn.execute(
        "INSERT INTO tasks (id, title, description, status, created_at, session_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            task.id.to_string(),
            task.title,
            task.description,
            task.status,
            task.created_at,
            task.session_id,
        ],
    )?;
    Ok(())
}

/// Returns all [`Task`]s, optionally filtered by `status`.
///
/// When `status` is `Some`, only tasks with the matching status are returned.
/// When `status` is `None`, all tasks are returned.
/// Results are ordered by `created_at` ascending.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn list(
    conn: &rusqlite::Connection,
    status: Option<&str>,
) -> Result<Vec<Task>, IngotError> {
    let rows: Vec<Task> = if let Some(st) = status {
        let mut stmt = conn.prepare(
            "SELECT id, title, description, status, created_at, session_id \
             FROM tasks WHERE status = ?1 ORDER BY created_at ASC",
        )?;
        let collected: Result<Vec<Task>, _> = stmt
            .query_map(rusqlite::params![st], row_to_task)?
            .collect();
        collected?
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, title, description, status, created_at, session_id \
             FROM tasks ORDER BY created_at ASC",
        )?;
        let collected: Result<Vec<Task>, _> = stmt.query_map([], row_to_task)?.collect();
        collected?
    };
    Ok(rows)
}

/// Updates the `status` field for the task with the given `id`.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the UPDATE fails.
pub(crate) fn update_status(
    conn: &rusqlite::Connection,
    id: &str,
    status: &str,
) -> Result<(), IngotError> {
    conn.execute(
        "UPDATE tasks SET status = ?1 WHERE id = ?2",
        rusqlite::params![status, id],
    )?;
    Ok(())
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Task {
        id,
        title: row.get(1)?,
        description: row.get(2)?,
        status: row.get(3)?,
        created_at: row.get(4)?,
        session_id: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ingot;

    fn sample_task(status: &str) -> Task {
        Task {
            id: Uuid::new_v4(),
            title: "Write tests".to_string(),
            description: "TDD red phase".to_string(),
            status: status.to_string(),
            created_at: 1_700_000_000.0,
            session_id: None,
        }
    }

    #[test]
    fn create_then_list_all_returns_task() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        let t = sample_task("planned");
        ingot.create_task(&t).unwrap();

        let results = ingot.list_tasks(None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, t.id);
        assert_eq!(results[0].title, "Write tests");
    }

    #[test]
    fn list_tasks_filters_by_status() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        ingot.create_task(&sample_task("planned")).unwrap();
        ingot.create_task(&sample_task("in_progress")).unwrap();
        ingot.create_task(&sample_task("planned")).unwrap();

        let planned = ingot.list_tasks(Some("planned")).unwrap();
        assert_eq!(planned.len(), 2);
        assert!(planned.iter().all(|t| t.status == "planned"));

        let in_progress = ingot.list_tasks(Some("in_progress")).unwrap();
        assert_eq!(in_progress.len(), 1);
    }

    #[test]
    fn update_task_status_changes_status() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        let t = sample_task("planned");
        ingot.create_task(&t).unwrap();

        ingot
            .update_task_status(&t.id.to_string(), "complete")
            .unwrap();

        let results = ingot.list_tasks(None).unwrap();
        assert_eq!(results[0].status, "complete");
    }

    #[test]
    fn list_tasks_empty_returns_empty_vec() {
        let ingot = Ingot::open_in_memory().unwrap();
        let results = ingot.list_tasks(None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn list_tasks_no_match_status_returns_empty() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        ingot.create_task(&sample_task("planned")).unwrap();

        let results = ingot.list_tasks(Some("complete")).unwrap();
        assert!(results.is_empty());
    }
}
