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

/// Converts a `rusqlite::Result<Checkpoint>` into the canonical
/// `Result<Option<Checkpoint>, IngotError>` used by query methods.
///
/// - `Ok(cp)` → `Ok(Some(cp))`
/// - `Err(QueryReturnedNoRows)` → `Ok(None)`
/// - any other error → `Err(IngotError::Db(e))`
fn optional_result(r: rusqlite::Result<Checkpoint>) -> Result<Option<Checkpoint>, IngotError> {
    match r {
        Ok(cp) => Ok(Some(cp)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(IngotError::Db(e)),
    }
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
    optional_result(conn.query_row(
        "SELECT id, session_id, turn_n, messages_json, created_at \
         FROM checkpoints WHERE session_id = ?1 AND turn_n = ?2",
        rusqlite::params![session_id, i64::from(turn_n)],
        row_to_checkpoint,
    ))
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
    optional_result(conn.query_row(
        "SELECT id, session_id, turn_n, messages_json, created_at \
         FROM checkpoints WHERE session_id = ?1 \
         ORDER BY turn_n DESC LIMIT 1",
        rusqlite::params![session_id],
        row_to_checkpoint,
    ))
}

/// Atomically rolls back a session to `turn_n`.
///
/// Within a single `SQLite` transaction:
/// 1. Loads the checkpoint at `turn_n`.
/// 2. Deletes all checkpoints with `turn_n > N`.
///
/// Returns `Ok(Some(checkpoint))` on success, `Ok(None)` when the target turn
/// does not exist (no changes are made in that case), or `Err` if the database
/// raises an error.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if any SQL operation fails.
pub fn rollback_session(
    conn: &rusqlite::Connection,
    session_id: &str,
    turn_n: u32,
) -> Result<Option<Checkpoint>, IngotError> {
    let tx = conn.unchecked_transaction()?;

    let cp_result: rusqlite::Result<Checkpoint> = tx.query_row(
        "SELECT id, session_id, turn_n, messages_json, created_at \
         FROM checkpoints WHERE session_id = ?1 AND turn_n = ?2",
        rusqlite::params![session_id, i64::from(turn_n)],
        row_to_checkpoint,
    );

    let checkpoint = match cp_result {
        Ok(c) => c,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            // Nothing to roll back — commit the no-op transaction cleanly.
            tx.commit()?;
            return Ok(None);
        }
        Err(e) => return Err(IngotError::Db(e)),
    };

    tx.execute(
        "DELETE FROM checkpoints WHERE session_id = ?1 AND turn_n > ?2",
        rusqlite::params![session_id, i64::from(turn_n)],
    )?;

    tx.commit()?;
    Ok(Some(checkpoint))
}

/// Returns all checkpoints for `session_id` ordered by `turn_n` ascending.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn list(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Vec<Checkpoint>, IngotError> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, turn_n, messages_json, created_at \
         FROM checkpoints WHERE session_id = ?1 ORDER BY turn_n ASC",
    )?;
    let rows: Result<Vec<Checkpoint>, _> = stmt
        .query_map(rusqlite::params![session_id], row_to_checkpoint)?
        .collect();
    Ok(rows?)
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

    #[test]
    fn list_checkpoints_returns_ordered() {
        let mut ig = Ingot::open_in_memory().unwrap();
        ig.save_checkpoint(&make_checkpoint("s1", 2)).unwrap();
        ig.save_checkpoint(&make_checkpoint("s1", 1)).unwrap();
        let cps = ig.list_checkpoints("s1").unwrap();
        assert_eq!(cps.len(), 2);
        assert_eq!(cps[0].turn_n, 1);
        assert_eq!(cps[1].turn_n, 2);
    }

    #[test]
    fn list_checkpoints_empty_for_unknown_session() {
        let ig = Ingot::open_in_memory().unwrap();
        let cps = ig.list_checkpoints("no-such-session").unwrap();
        assert!(cps.is_empty());
    }

    #[test]
    fn save_and_load_checkpoint_round_trip() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        let messages_json = r#"[{"role":"user","content":"round-trip"}]"#;
        let cp = Checkpoint {
            id: Uuid::new_v4(),
            session_id: "rt-session".to_string(),
            turn_n: 7,
            messages_json: messages_json.to_string(),
            created_at: 1_700_001_000.0,
        };
        ingot.save_checkpoint(&cp).unwrap();

        let loaded = ingot.load_checkpoint("rt-session", 7).unwrap().unwrap();
        assert_eq!(loaded.id, cp.id);
        assert_eq!(loaded.turn_n, 7);
        assert_eq!(loaded.messages_json, messages_json);
        assert_eq!(loaded.session_id, "rt-session");
    }

    #[test]
    fn rollback_discards_later_turns() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        ingot
            .save_checkpoint(&make_checkpoint("rollback-sess", 1))
            .unwrap();
        ingot
            .save_checkpoint(&make_checkpoint("rollback-sess", 2))
            .unwrap();
        ingot
            .save_checkpoint(&make_checkpoint("rollback-sess", 3))
            .unwrap();

        // Load the turn-1 checkpoint — simulates rolling back before turns 2 and 3.
        let rolled_back = ingot.load_checkpoint("rollback-sess", 1).unwrap().unwrap();
        assert_eq!(rolled_back.turn_n, 1);

        // Turns 2 and 3 still exist in the store; the caller is responsible for
        // discarding them.  Verify they are independently accessible.
        let turn2 = ingot.load_checkpoint("rollback-sess", 2).unwrap().unwrap();
        assert_eq!(turn2.turn_n, 2);

        let cps = ingot.list_checkpoints("rollback-sess").unwrap();
        assert_eq!(cps.len(), 3);
        // Confirm the roll-back point is the first in the ordered list.
        assert_eq!(cps[0].turn_n, 1);
    }

    /// Builds an in-memory connection with the checkpoints table for direct
    /// `rollback_session` tests (which call the `pub(crate)` fn without going
    /// through `Ingot`).
    fn in_memory_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS checkpoints (
                 id            TEXT PRIMARY KEY,
                 session_id    TEXT NOT NULL,
                 turn_n        INTEGER NOT NULL,
                 messages_json TEXT NOT NULL,
                 created_at    REAL NOT NULL,
                 UNIQUE(session_id, turn_n)
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn rollback_deletes_later_checkpoints() {
        let conn = in_memory_conn();

        // Insert turns 1, 2, 3 directly.
        for turn in 1i64..=3 {
            let cp = make_checkpoint("sess", turn);
            conn.execute(
                "INSERT INTO checkpoints (id, session_id, turn_n, messages_json, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    cp.id.to_string(),
                    cp.session_id,
                    cp.turn_n,
                    cp.messages_json,
                    cp.created_at,
                ],
            )
            .unwrap();
        }

        // Roll back to turn 1: turns 2 and 3 must be deleted.
        let result = rollback_session(&conn, "sess", 1).unwrap();
        assert!(result.is_some(), "must return the turn-1 checkpoint");
        assert_eq!(result.unwrap().turn_n, 1);

        // Only turn 1 remains.
        let remaining: Vec<i64> = {
            let mut stmt = conn
                .prepare(
                    "SELECT turn_n FROM checkpoints WHERE session_id = 'sess' ORDER BY turn_n ASC",
                )
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(
            remaining,
            vec![1i64],
            "only turn 1 must remain after rollback"
        );
    }

    #[test]
    fn rollback_unknown_turn_returns_none() {
        let conn = in_memory_conn();

        // Insert turn 1 but request rollback to non-existent turn 99.
        let cp = make_checkpoint("sess2", 1);
        conn.execute(
            "INSERT INTO checkpoints (id, session_id, turn_n, messages_json, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                cp.id.to_string(),
                cp.session_id,
                cp.turn_n,
                cp.messages_json,
                cp.created_at,
            ],
        )
        .unwrap();

        let result = rollback_session(&conn, "sess2", 99).unwrap();
        assert!(
            result.is_none(),
            "unknown turn must return None without modifying data"
        );

        // Turn 1 must still be intact.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM checkpoints WHERE session_id = 'sess2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "existing checkpoints must be unchanged when target turn not found"
        );
    }
}
