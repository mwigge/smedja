use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::IngotError;

/// A per-turn token cost record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostEntry {
    /// Unique entry identifier (UUID v4 stored as TEXT).
    pub id: Uuid,
    /// Owning session identifier.
    pub session_id: String,
    /// Turn index within the session.
    pub turn_n: i64,
    /// Runner that incurred the cost: `"claude"`, `"local"`, or `"gemini"`.
    pub runner: String,
    /// Model identifier.
    pub model: String,
    /// Input token count.
    pub input_tok: i64,
    /// Output token count.
    pub output_tok: i64,
    /// Monetary cost in USD.
    pub cost_usd: f64,
    /// Unix epoch timestamp when the entry was recorded.
    pub created_at: f64,
}

/// Inserts a new [`CostEntry`] row.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the INSERT fails.
pub(crate) fn insert(conn: &rusqlite::Connection, entry: &CostEntry) -> Result<(), IngotError> {
    conn.execute(
        "INSERT INTO cost_ledger \
         (id, session_id, turn_n, runner, model, input_tok, output_tok, cost_usd, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            entry.id.to_string(),
            entry.session_id,
            entry.turn_n,
            entry.runner,
            entry.model,
            entry.input_tok,
            entry.output_tok,
            entry.cost_usd,
            entry.created_at,
        ],
    )?;
    Ok(())
}

/// Returns the sum of `cost_usd` for all entries belonging to `session_id`.
///
/// Returns `0.0` when no entries exist for the session.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn session_total(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<f64, IngotError> {
    let total: f64 = conn.query_row(
        "SELECT COALESCE(SUM(cost_usd), 0.0) FROM cost_ledger WHERE session_id = ?1",
        rusqlite::params![session_id],
        |row| row.get(0),
    )?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ingot;

    fn make_entry(session_id: &str, cost_usd: f64, turn_n: i64) -> CostEntry {
        CostEntry {
            id: Uuid::new_v4(),
            session_id: session_id.to_string(),
            turn_n,
            runner: "claude".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            input_tok: 200,
            output_tok: 100,
            cost_usd,
            created_at: 1_700_000_000.0,
        }
    }

    #[test]
    fn insert_then_session_cost_returns_sum() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        ingot.insert_cost(&make_entry("s1", 0.001, 0)).unwrap();
        ingot.insert_cost(&make_entry("s1", 0.002, 1)).unwrap();
        ingot.insert_cost(&make_entry("s1", 0.003, 2)).unwrap();

        let total = ingot.session_cost("s1").unwrap();
        assert!(
            (total - 0.006_f64).abs() < 1e-9,
            "expected 0.006, got {total}"
        );
    }

    #[test]
    fn session_cost_with_no_entries_returns_zero() {
        let ingot = Ingot::open_in_memory().unwrap();
        let total = ingot.session_cost("no-session").unwrap();
        assert!(total.abs() < f64::EPSILON);
    }

    #[test]
    fn session_cost_scoped_to_session() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        ingot.insert_cost(&make_entry("s1", 0.010, 0)).unwrap();
        ingot.insert_cost(&make_entry("s2", 0.100, 0)).unwrap();

        let total_s1 = ingot.session_cost("s1").unwrap();
        assert!((total_s1 - 0.010_f64).abs() < 1e-9);
    }

    #[test]
    fn insert_cost_round_trips_all_fields() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        let entry = CostEntry {
            id: Uuid::new_v4(),
            session_id: "s-rt".to_string(),
            turn_n: 7,
            runner: "gemini".to_string(),
            model: "gemini-2.5-pro".to_string(),
            input_tok: 500,
            output_tok: 250,
            cost_usd: 0.042,
            created_at: 1_720_000_000.0,
        };
        ingot.insert_cost(&entry).unwrap();
        let total = ingot.session_cost("s-rt").unwrap();
        assert!((total - 0.042_f64).abs() < 1e-9);
    }
}
