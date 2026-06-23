use serde::{Deserialize, Serialize};
use smedja_types::Timestamp;
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
    ///
    /// Compaction checkpoints historically used `-1` as a sentinel; they are now
    /// distinguished by a non-`None` [`Checkpoint::compaction_id`] instead, so
    /// multiple compactions per session are retained rather than overwritten.
    pub turn_n: i64,
    /// JSON-serialised array of message objects.
    pub messages_json: String,
    /// Timestamp when the checkpoint was saved (micros since the Unix epoch).
    pub created_at: Timestamp,
    /// Compaction discriminator.
    ///
    /// `None` for ordinary per-turn checkpoints (one row per `(session_id,
    /// turn_n)`). `Some(id)` for compaction checkpoints, where `id` is a unique
    /// string (e.g. a UUID) that keeps every compaction distinct so a session
    /// can retain its full compaction history.
    #[serde(default)]
    pub compaction_id: Option<String>,
}

/// Inserts or replaces a [`Checkpoint`].
///
/// Ordinary per-turn checkpoints (`compaction_id == None`) replace any existing
/// row for the same `(session_id, turn_n)` via the partial unique index, keeping
/// saves idempotent. Compaction checkpoints (`compaction_id == Some(_)`) carry no
/// turn-based uniqueness, so every compaction is retained as a distinct row.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the upsert fails.
pub(crate) fn save(conn: &rusqlite::Connection, cp: &Checkpoint) -> Result<(), IngotError> {
    conn.execute(
        "INSERT OR REPLACE INTO checkpoints \
         (id, session_id, turn_n, messages_json, created_at, compaction_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            cp.id.to_string(),
            cp.session_id,
            cp.turn_n,
            cp.messages_json,
            cp.created_at.as_micros(),
            cp.compaction_id,
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

/// Column list shared by all SELECT statements in this module.
const SELECT_COLS: &str = "id, session_id, turn_n, messages_json, created_at, compaction_id";

/// Retrieves an ordinary [`Checkpoint`] by `session_id` and `turn_n`, returning
/// `None` if not found. Compaction checkpoints are excluded from this lookup.
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
        &format!(
            "SELECT {SELECT_COLS} FROM checkpoints \
             WHERE session_id = ?1 AND turn_n = ?2 AND compaction_id IS NULL"
        ),
        rusqlite::params![session_id, i64::from(turn_n)],
        row_to_checkpoint,
    ))
}

/// Returns the ordinary checkpoint with the highest `turn_n` for `session_id`,
/// or `None` if no checkpoints exist. Compaction checkpoints are excluded.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn latest(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Option<Checkpoint>, IngotError> {
    optional_result(conn.query_row(
        &format!(
            "SELECT {SELECT_COLS} FROM checkpoints \
             WHERE session_id = ?1 AND compaction_id IS NULL \
             ORDER BY turn_n DESC LIMIT 1"
        ),
        rusqlite::params![session_id],
        row_to_checkpoint,
    ))
}

/// Atomically rolls back a session to `turn_n`.
///
/// Within a single `SQLite` transaction:
/// 1. Loads the ordinary checkpoint at `turn_n`.
/// 2. Deletes all ordinary checkpoints with `turn_n > N`.
///
/// Compaction checkpoints are never loaded or deleted by this operation.
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
        &format!(
            "SELECT {SELECT_COLS} FROM checkpoints \
             WHERE session_id = ?1 AND turn_n = ?2 AND compaction_id IS NULL"
        ),
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
        "DELETE FROM checkpoints \
         WHERE session_id = ?1 AND turn_n > ?2 AND compaction_id IS NULL",
        rusqlite::params![session_id, i64::from(turn_n)],
    )?;

    tx.commit()?;
    Ok(Some(checkpoint))
}

/// Returns all ordinary checkpoints for `session_id` ordered by `turn_n`
/// ascending. Compaction checkpoints are excluded.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn list(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Vec<Checkpoint>, IngotError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM checkpoints \
         WHERE session_id = ?1 AND compaction_id IS NULL ORDER BY turn_n ASC"
    ))?;
    let rows: Result<Vec<Checkpoint>, _> = stmt
        .query_map(rusqlite::params![session_id], row_to_checkpoint)?
        .collect();
    Ok(rows?)
}

/// Returns all compaction checkpoints for `session_id` ordered by `created_at`
/// ascending. Ordinary per-turn checkpoints are excluded.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn list_compactions(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Vec<Checkpoint>, IngotError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM checkpoints \
         WHERE session_id = ?1 AND compaction_id IS NOT NULL ORDER BY created_at ASC"
    ))?;
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
        created_at: Timestamp::from_micros(crate::read_micros(row, 4)?),
        compaction_id: row.get(5)?,
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
            created_at: Timestamp::from_secs_f64(
                1_700_000_000.0 + f64::from(u32::try_from(turn_n).unwrap_or(0)),
            ),
            compaction_id: None,
        }
    }

    fn make_compaction(session_id: &str, compaction_id: &str, created_secs: f64) -> Checkpoint {
        Checkpoint {
            id: Uuid::new_v4(),
            session_id: session_id.to_string(),
            turn_n: -1,
            messages_json: r#"[{"role":"system","content":"compacted"}]"#.to_string(),
            created_at: Timestamp::from_secs_f64(created_secs),
            compaction_id: Some(compaction_id.to_string()),
        }
    }

    #[test]
    fn save_then_load_returns_checkpoint() {
        let ingot = Ingot::open_in_memory().unwrap();
        let cp = make_checkpoint("sess-1", 0);
        ingot.save_checkpoint(&cp).unwrap();

        let loaded = ingot.load_checkpoint("sess-1", 0).unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.id, cp.id);
        assert_eq!(loaded.turn_n, 0);
        assert_eq!(loaded.messages_json, cp.messages_json);
        assert!(loaded.compaction_id.is_none());
    }

    #[test]
    fn load_nonexistent_returns_none() {
        let ingot = Ingot::open_in_memory().unwrap();
        let result = ingot.load_checkpoint("no-session", 99).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn latest_checkpoint_returns_highest_turn() {
        let ingot = Ingot::open_in_memory().unwrap();
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
        let ingot = Ingot::open_in_memory().unwrap();
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
        // Only one ordinary row remains for the turn.
        assert_eq!(ingot.list_checkpoints("s").unwrap().len(), 1);
    }

    #[test]
    fn latest_checkpoint_scoped_to_session() {
        let ingot = Ingot::open_in_memory().unwrap();
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
        let ig = Ingot::open_in_memory().unwrap();
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
        let ingot = Ingot::open_in_memory().unwrap();
        let messages_json = r#"[{"role":"user","content":"round-trip"}]"#;
        let cp = Checkpoint {
            id: Uuid::new_v4(),
            session_id: "rt-session".to_string(),
            turn_n: 7,
            messages_json: messages_json.to_string(),
            created_at: Timestamp::from_secs_f64(1_700_001_000.0),
            compaction_id: None,
        };
        ingot.save_checkpoint(&cp).unwrap();

        let loaded = ingot.load_checkpoint("rt-session", 7).unwrap().unwrap();
        assert_eq!(loaded.id, cp.id);
        assert_eq!(loaded.turn_n, 7);
        assert_eq!(loaded.messages_json, messages_json);
        assert_eq!(loaded.session_id, "rt-session");
        assert_eq!(loaded.created_at, Timestamp::from_secs_f64(1_700_001_000.0));
    }

    #[test]
    fn two_compactions_for_one_session_are_both_retained() {
        let ingot = Ingot::open_in_memory().unwrap();
        let first = make_compaction("compact-sess", "compaction-1", 1_700_000_000.0);
        let second = make_compaction("compact-sess", "compaction-2", 1_700_000_100.0);

        ingot.save_checkpoint(&first).unwrap();
        ingot.save_checkpoint(&second).unwrap();

        let compactions = ingot.list_compaction_checkpoints("compact-sess").unwrap();
        assert_eq!(compactions.len(), 2, "both compactions must be retained");

        let ids: Vec<&str> = compactions
            .iter()
            .filter_map(|c| c.compaction_id.as_deref())
            .collect();
        assert!(ids.contains(&"compaction-1"));
        assert!(ids.contains(&"compaction-2"));
    }

    #[test]
    fn compaction_checkpoints_do_not_collide_with_turns() {
        let ingot = Ingot::open_in_memory().unwrap();
        // A normal turn 0 alongside two compactions.
        ingot.save_checkpoint(&make_checkpoint("mixed", 0)).unwrap();
        ingot
            .save_checkpoint(&make_compaction("mixed", "c-a", 1.0))
            .unwrap();
        ingot
            .save_checkpoint(&make_compaction("mixed", "c-b", 2.0))
            .unwrap();

        // Ordinary listing excludes compactions.
        let turns = ingot.list_checkpoints("mixed").unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].turn_n, 0);

        // Compaction listing excludes ordinary turns.
        let compactions = ingot.list_compaction_checkpoints("mixed").unwrap();
        assert_eq!(compactions.len(), 2);
    }

    /// Builds an in-memory connection with the checkpoints table for direct
    /// `rollback_session` tests (which call the `pub(crate)` fn without going
    /// through `Ingot`).
    fn in_memory_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS checkpoints (
                 id            TEXT PRIMARY KEY,
                 session_id    TEXT NOT NULL,
                 turn_n        INTEGER NOT NULL,
                 messages_json TEXT NOT NULL,
                 created_at    INTEGER NOT NULL,
                 compaction_id TEXT
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_checkpoints_turn \
                 ON checkpoints(session_id, turn_n) WHERE compaction_id IS NULL;",
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
                "INSERT INTO checkpoints \
                 (id, session_id, turn_n, messages_json, created_at, compaction_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    cp.id.to_string(),
                    cp.session_id,
                    cp.turn_n,
                    cp.messages_json,
                    cp.created_at.as_micros(),
                    cp.compaction_id,
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
            "INSERT INTO checkpoints \
             (id, session_id, turn_n, messages_json, created_at, compaction_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                cp.id.to_string(),
                cp.session_id,
                cp.turn_n,
                cp.messages_json,
                cp.created_at.as_micros(),
                cp.compaction_id,
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
