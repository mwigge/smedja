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
    /// Full response text stored once the turn completes.
    pub response: Option<String>,
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
    // A single query handles both the filtered and unfiltered cases.
    // When `status_param` is NULL, the `?1 IS NULL` branch matches every row.
    let status_param: rusqlite::types::Value = match status {
        Some(s) => rusqlite::types::Value::Text(s.to_owned()),
        None => rusqlite::types::Value::Null,
    };
    let mut stmt = conn.prepare(
        "SELECT id, title, description, status, created_at, session_id, response \
         FROM tasks \
         WHERE (?1 IS NULL OR status = ?1) \
         ORDER BY created_at ASC",
    )?;
    let collected: Result<Vec<Task>, _> = stmt
        .query_map(rusqlite::params![status_param], row_to_task)?
        .collect();
    Ok(collected?)
}

/// Retrieves a single [`Task`] by `id`, returning `None` when not found.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn get(conn: &rusqlite::Connection, id: &str) -> Result<Option<Task>, IngotError> {
    let mut stmt = conn.prepare(
        "SELECT id, title, description, status, created_at, session_id, response \
         FROM tasks WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(rusqlite::params![id], row_to_task)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
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

/// Stores the full response text and transitions the task to `"complete"`.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the UPDATE fails.
pub(crate) fn update_response(
    conn: &rusqlite::Connection,
    id: &str,
    response: &str,
) -> Result<(), IngotError> {
    conn.execute(
        "UPDATE tasks SET response = ?1, status = 'complete' WHERE id = ?2",
        rusqlite::params![response, id],
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
        response: row.get(6)?,
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
            response: None,
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

    #[test]
    fn get_task_returns_task_by_id() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        let t = sample_task("planned");
        ingot.create_task(&t).unwrap();

        let found = ingot.get_task(&t.id.to_string()).unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.id, t.id);
        assert_eq!(found.title, "Write tests");
        assert!(found.response.is_none());
    }

    #[test]
    fn get_task_returns_none_for_missing() {
        let ingot = Ingot::open_in_memory().unwrap();
        let found = ingot
            .get_task("00000000-0000-0000-0000-000000000000")
            .unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn set_task_response_stores_response_and_marks_complete() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        let t = sample_task("in_progress");
        ingot.create_task(&t).unwrap();

        ingot
            .set_task_response(&t.id.to_string(), "The answer is 42.")
            .unwrap();

        let updated = ingot.get_task(&t.id.to_string()).unwrap().unwrap();
        assert_eq!(updated.status, "complete");
        assert_eq!(updated.response.as_deref(), Some("The answer is 42."));
    }

    #[test]
    fn task_lifecycle_create_update_get() {
        use crate::session::Session;

        let mut ingot = Ingot::open_in_memory().unwrap();

        // Create a session so the task carries a meaningful session_id.
        let session = Session {
            id: Uuid::new_v4(),
            created_at: 1_700_000_000.0,
            updated_at: 1_700_000_000.0,
            status: "active".to_string(),
            task_id: None,
            mode: None,
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
        };
        ingot.create_session(&session).unwrap();

        // Create a task linked to the session.
        let task = Task {
            id: Uuid::new_v4(),
            title: "Fix the softcap".to_string(),
            description: String::new(),
            status: "planned".to_string(),
            created_at: 1_700_000_001.0,
            session_id: Some(session.id.to_string()),
            response: None,
        };
        ingot.create_task(&task).unwrap();

        // get_task returns the correct task with the expected initial fields.
        let loaded = ingot
            .get_task(&task.id.to_string())
            .unwrap()
            .expect("task must exist after create");
        assert_eq!(loaded.title, "Fix the softcap");
        assert_eq!(loaded.status, "planned");
        assert_eq!(
            loaded.session_id.as_deref(),
            Some(session.id.to_string().as_str())
        );

        // Update the status to "complete".
        ingot
            .update_task_status(&task.id.to_string(), "complete")
            .unwrap();

        // get_task reflects the updated status.
        let updated = ingot
            .get_task(&task.id.to_string())
            .unwrap()
            .expect("task must still exist after status update");
        assert_eq!(updated.status, "complete");

        // list_tasks(None) includes the task; filter by session_id to isolate it.
        let all_tasks = ingot.list_tasks(None).unwrap();
        assert!(
            all_tasks.iter().any(|t| t.title == "Fix the softcap"),
            "list_tasks must include the created task"
        );
    }
}
