use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::IngotError;

/// A durable snapshot of a conversation turn, enabling rollback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Unique checkpoint identifier (UUID v4 stored as TEXT).
    pub id: Uuid,
    /// Session this checkpoint belongs to.
    pub session_id: String,
    /// Monotonically increasing turn index within the session.
    pub turn_n: i64,
    /// JSON-serialised array of message objects.
    pub messages_json: String,
    /// Unix epoch timestamp when the checkpoint was saved.
    pub created_at: f64,
}

/// Inserts or replaces a [`Checkpoint`].
///
/// The `UNIQUE(session_id, turn_n)` constraint means saving the same turn a second
/// time will fail unless using `INSERT OR REPLACE`. This method uses `INSERT OR REPLACE`
/// to allow idempotent saves.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the upsert fails.
pub(crate) fn save(conn: &rusqlite::Connection, cp: &Checkpoint) -> Result<(), IngotError> {
    conn.execute(
        "INSERT OR REPLACE INTO checkpoints \
         (id, session_id, turn_n, messages_json, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            cp.id.to_string(),
            cp.session_id,
            cp.turn_n,
            cp.messages_json,
            cp.created_at,
        ],
    )?;
    Ok(())
}

/// Retrieves a [`Checkpoint`] by `session_id` and `turn_n`, returning `None` if not found.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn load(
    conn: &rusqlite::Connection,
    session_id: &str,
    turn_n: u32,
) -> Result<Option<Checkpoint>, IngotError> {
    let result = conn.query_row(
        "SELECT id, session_id, turn_n, messages_json, created_at \
         FROM checkpoints WHERE session_id = ?1 AND turn_n = ?2",
        rusqlite::params![session_id, i64::from(turn_n)],
        row_to_checkpoint,
    );

    match result {
        Ok(cp) => Ok(Some(cp)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(IngotError::Db(e)),
    }
}

/// Returns the checkpoint with the highest `turn_n` for `session_id`, or `None` if
/// no checkpoints exist.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn latest(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Option<Checkpoint>, IngotError> {
    let result = conn.query_row(
        "SELECT id, session_id, turn_n, messages_json, created_at \
         FROM checkpoints WHERE session_id = ?1 \
         ORDER BY turn_n DESC LIMIT 1",
        rusqlite::params![session_id],
        row_to_checkpoint,
    );

    match result {
        Ok(cp) => Ok(Some(cp)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(IngotError::Db(e)),
    }
}

fn row_to_checkpoint(row: &rusqlite::Row<'_>) -> rusqlite::Result<Checkpoint> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Checkpoint {
        id,
        session_id: row.get(1)?,
        turn_n: row.get(2)?,
        messages_json: row.get(3)?,
        created_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ingot;

    fn make_checkpoint(session_id: &str, turn_n: i64) -> Checkpoint {
        Checkpoint {
            id: Uuid::new_v4(),
            session_id: session_id.to_string(),
            turn_n,
            messages_json: r#"[{"role":"user","content":"hello"}]"#.to_string(),
            created_at: 1_700_000_000.0 + f64::from(u32::try_from(turn_n).unwrap_or(0)),
        }
    }

    #[test]
    fn save_then_load_returns_checkpoint() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        let cp = make_checkpoint("sess-1", 0);
        ingot.save_checkpoint(&cp).unwrap();

        let loaded = ingot.load_checkpoint("sess-1", 0).unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.id, cp.id);
        assert_eq!(loaded.turn_n, 0);
        assert_eq!(loaded.messages_json, cp.messages_json);
    }

    #[test]
    fn load_nonexistent_returns_none() {
        let ingot = Ingot::open_in_memory().unwrap();
        let result = ingot.load_checkpoint("no-session", 99).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn latest_checkpoint_returns_highest_turn() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        ingot.save_checkpoint(&make_checkpoint("s", 0)).unwrap();
        ingot.save_checkpoint(&make_checkpoint("s", 2)).unwrap();
        ingot.save_checkpoint(&make_checkpoint("s", 1)).unwrap();

        let latest = ingot.latest_checkpoint("s").unwrap();
        assert!(latest.is_some());
        assert_eq!(latest.unwrap().turn_n, 2);
    }

    #[test]
    fn latest_checkpoint_no_data_returns_none() {
        let ingot = Ingot::open_in_memory().unwrap();
        let result = ingot.latest_checkpoint("empty-session").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn save_is_idempotent_for_same_turn() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        let cp1 = make_checkpoint("s", 0);
        let mut cp2 = make_checkpoint("s", 0);
        cp2.messages_json = r#"[{"role":"assistant","content":"updated"}]"#.to_string();

        ingot.save_checkpoint(&cp1).unwrap();
        ingot.save_checkpoint(&cp2).unwrap();

        let loaded = ingot.load_checkpoint("s", 0).unwrap().unwrap();
        assert_eq!(
            loaded.messages_json,
            r#"[{"role":"assistant","content":"updated"}]"#
        );
    }

    #[test]
    fn latest_checkpoint_scoped_to_session() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        ingot
            .save_checkpoint(&make_checkpoint("session-a", 5))
            .unwrap();
        ingot
            .save_checkpoint(&make_checkpoint("session-b", 1))
            .unwrap();

        let latest_a = ingot.latest_checkpoint("session-a").unwrap().unwrap();
        assert_eq!(latest_a.turn_n, 5);

        let latest_b = ingot.latest_checkpoint("session-b").unwrap().unwrap();
        assert_eq!(latest_b.turn_n, 1);
    }
}
