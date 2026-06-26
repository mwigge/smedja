//! Per-turn token snapshot persistence.
//!
//! Records input and output token counts for each turn, together with
//! running cumulative totals. This enables the session cost estimator to
//! determine how close a session is to its context-window limit without
//! re-scanning all audit events.

use smedja_types::Timestamp;
use uuid::Uuid;

use crate::error::IngotError;

/// Token usage snapshot for a single conversation turn.
#[derive(Debug, Clone)]
pub struct TokenSnapshot {
    /// Unique snapshot identifier.
    pub id: Uuid,
    /// Session this snapshot belongs to.
    pub session_id: String,
    /// Turn index within the session (matches `checkpoints.turn_n`).
    pub turn_n: i64,
    /// Input tokens consumed in this turn.
    pub input_tok: i64,
    /// Output tokens produced in this turn.
    pub output_tok: i64,
    /// Cumulative input tokens across all turns in the session up to and
    /// including this one.
    pub cumulative_input: i64,
    /// Cumulative output tokens across all turns in the session up to and
    /// including this one.
    pub cumulative_output: i64,
    /// Timestamp when the snapshot was recorded (micros since the Unix epoch).
    pub created_at: Timestamp,
}

/// Inserts a [`TokenSnapshot`] into the `turn_token_snapshots` table.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the INSERT fails.
pub(crate) fn save(conn: &rusqlite::Connection, snap: &TokenSnapshot) -> Result<(), IngotError> {
    conn.execute(
        "INSERT OR REPLACE INTO turn_token_snapshots \
         (id, session_id, turn_n, input_tok, output_tok, \
          cumulative_input, cumulative_output, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            snap.id.to_string(),
            snap.session_id,
            snap.turn_n,
            snap.input_tok,
            snap.output_tok,
            snap.cumulative_input,
            snap.cumulative_output,
            snap.created_at.as_micros(),
        ],
    )?;
    Ok(())
}

/// Returns all [`TokenSnapshot`]s for `session_id`, ordered by `turn_n`
/// ascending.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn list_by_session(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Vec<TokenSnapshot>, IngotError> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, turn_n, input_tok, output_tok, \
                cumulative_input, cumulative_output, created_at \
         FROM turn_token_snapshots \
         WHERE session_id = ?1 \
         ORDER BY turn_n ASC",
    )?;
    let rows: Result<Vec<TokenSnapshot>, _> = stmt
        .query_map(rusqlite::params![session_id], row_to_snapshot)?
        .collect();
    Ok(rows?)
}

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<TokenSnapshot> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(TokenSnapshot {
        id,
        session_id: row.get(1)?,
        turn_n: row.get(2)?,
        input_tok: row.get(3)?,
        output_tok: row.get(4)?,
        cumulative_input: row.get(5)?,
        cumulative_output: row.get(6)?,
        created_at: Timestamp::from_micros(crate::read_micros(row, 7)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ingot;

    fn make_snap(session_id: &str, turn_n: i64, input: i64, output: i64) -> TokenSnapshot {
        TokenSnapshot {
            id: Uuid::new_v4(),
            session_id: session_id.to_owned(),
            turn_n,
            input_tok: input,
            output_tok: output,
            cumulative_input: input,   // set properly per test
            cumulative_output: output, // set properly per test
            #[allow(clippy::cast_precision_loss)] // test-only; turn_n is small
            created_at: Timestamp::from_secs_f64(1_700_000_000.0 + turn_n as f64),
        }
    }

    #[test]
    fn save_and_list_returns_ordered_snapshots() {
        let ingot = Ingot::open_in_memory().unwrap();

        let s1 = make_snap("sess", 2, 100, 50);
        let s2 = make_snap("sess", 0, 10, 5);
        let s3 = make_snap("sess", 1, 20, 10);

        ingot.save_token_snapshot(&s1).unwrap();
        ingot.save_token_snapshot(&s2).unwrap();
        ingot.save_token_snapshot(&s3).unwrap();

        let snaps = ingot.session_token_snapshots("sess").unwrap();
        assert_eq!(snaps.len(), 3);
        assert_eq!(snaps[0].turn_n, 0);
        assert_eq!(snaps[1].turn_n, 1);
        assert_eq!(snaps[2].turn_n, 2);
    }

    #[test]
    fn cumulative_totals_accumulate_correctly() {
        let ingot = Ingot::open_in_memory().unwrap();

        let mut snap1 = make_snap("acc", 0, 100, 50);
        snap1.cumulative_input = 100;
        snap1.cumulative_output = 50;
        ingot.save_token_snapshot(&snap1).unwrap();

        let mut snap2 = make_snap("acc", 1, 200, 80);
        snap2.cumulative_input = 300;
        snap2.cumulative_output = 130;
        ingot.save_token_snapshot(&snap2).unwrap();

        let acc_snaps = ingot.session_token_snapshots("acc").unwrap();
        assert_eq!(acc_snaps.len(), 2);

        // Last snapshot should reflect cumulative state.
        let last = &acc_snaps[1];
        assert_eq!(last.cumulative_input, 300);
        assert_eq!(last.cumulative_output, 130);
    }

    #[test]
    fn empty_session_returns_no_snapshots() {
        let ingot = Ingot::open_in_memory().unwrap();
        let snaps = ingot.session_token_snapshots("no-such-session").unwrap();
        assert!(snaps.is_empty());
    }
}
