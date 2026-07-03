use smedja_types::Timestamp;

use crate::error::IngotError;

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
