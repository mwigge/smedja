use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::IngotError;

/// A per-model/runner aggregate row returned by [`session_cost_entries`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRow {
    /// Model identifier (e.g. `"claude-sonnet-4-6"`).
    pub model: String,
    /// Runner name (e.g. `"claude-cli"`, `"local"`).
    pub runner: String,
    /// Number of turns that contributed to this row.
    pub turns: i64,
    /// Total input tokens across all turns.
    pub input_tok: i64,
    /// Total output tokens across all turns.
    pub output_tok: i64,
    /// Total monetary cost in USD.
    pub cost_usd: f64,
}

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

/// Returns the model name from the most recent cost entry for `session_id`.
///
/// Returns `None` when no entries exist.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn last_model(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Option<String>, IngotError> {
    let mut stmt = conn.prepare(
        "SELECT model FROM cost_ledger WHERE session_id = ?1 ORDER BY turn_n DESC LIMIT 1",
    )?;
    let result = stmt
        .query_row(rusqlite::params![session_id], |row| row.get(0))
        .optional()?;
    Ok(result)
}

/// Returns per-model/runner aggregate rows for a session, ordered by descending cost.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn session_cost_entries(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Vec<CostRow>, IngotError> {
    let mut stmt = conn.prepare(
        "SELECT model, runner, COUNT(*) AS turns, \
         SUM(input_tok) AS input_tok, SUM(output_tok) AS output_tok, \
         SUM(cost_usd) AS cost_usd \
         FROM cost_ledger \
         WHERE session_id = ?1 \
         GROUP BY model, runner \
         ORDER BY cost_usd DESC",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![session_id], |row| {
            Ok(CostRow {
                model: row.get(0)?,
                runner: row.get(1)?,
                turns: row.get(2)?,
                input_tok: row.get(3)?,
                output_tok: row.get(4)?,
                cost_usd: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
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

    fn make_entry_with(
        session_id: &str,
        model: &str,
        runner: &str,
        cost_usd: f64,
        turn_n: i64,
    ) -> CostEntry {
        CostEntry {
            id: Uuid::new_v4(),
            session_id: session_id.to_string(),
            turn_n,
            runner: runner.to_string(),
            model: model.to_string(),
            input_tok: 200,
            output_tok: 100,
            cost_usd,
            created_at: 1_700_000_000.0,
        }
    }

    #[test]
    fn session_cost_entries_empty_returns_empty_vec() {
        let ingot = Ingot::open_in_memory().unwrap();
        let rows = ingot.session_cost_entries("no-session").unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn session_cost_entries_single_model_aggregates() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        ingot
            .insert_cost(&make_entry_with(
                "s1",
                "claude-sonnet-4-6",
                "claude-cli",
                0.010,
                0,
            ))
            .unwrap();
        ingot
            .insert_cost(&make_entry_with(
                "s1",
                "claude-sonnet-4-6",
                "claude-cli",
                0.020,
                1,
            ))
            .unwrap();
        let rows = ingot.session_cost_entries("s1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].turns, 2);
        assert!(
            (rows[0].cost_usd - 0.030_f64).abs() < 1e-9,
            "expected 0.030; got {}",
            rows[0].cost_usd
        );
    }

    #[test]
    fn session_cost_entries_multiple_models_sorted_descending() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        ingot
            .insert_cost(&make_entry_with("s2", "gpt-4o-mini", "codex-cli", 0.001, 0))
            .unwrap();
        ingot
            .insert_cost(&make_entry_with(
                "s2",
                "claude-sonnet-4-6",
                "claude-cli",
                0.100,
                1,
            ))
            .unwrap();
        let rows = ingot.session_cost_entries("s2").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].model, "claude-sonnet-4-6",
            "most expensive model should be first"
        );
    }

    #[test]
    fn session_cost_entries_scoped_to_session() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        ingot
            .insert_cost(&make_entry_with("s3", "gpt-4o", "openai", 0.050, 0))
            .unwrap();
        ingot
            .insert_cost(&make_entry_with("s4", "gpt-4o", "openai", 0.999, 0))
            .unwrap();
        let rows = ingot.session_cost_entries("s3").unwrap();
        assert_eq!(rows.len(), 1);
        assert!((rows[0].cost_usd - 0.050_f64).abs() < 1e-9);
    }
}
