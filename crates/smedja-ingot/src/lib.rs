//! `smedja-ingot` — `SQLite` persistence layer for the smedja multi-agent orchestration platform.
//!
//! Provides schema bootstrap, CRUD operations for audit events, sessions, tasks,
//! checkpoints, and cost ledger entries. All operations are synchronous; callers
//! running inside an async runtime should use [`tokio::task::spawn_blocking`] to
//! avoid blocking the executor thread.

pub mod audit;
pub mod checkpoint;
pub mod cost;
pub mod error;
pub mod guard;
pub mod handle;
pub mod loop_state;
pub mod mcp;
pub mod methodology;
pub mod metrics_rollup;
mod migrations;
pub mod openspec_store;
mod ops;
pub mod prompt_hash;
pub mod savings_rollup;
pub mod session;
pub mod task;
pub mod token_snapshot;

pub use audit::AuditEvent;
pub use checkpoint::Checkpoint;
pub use cost::{CostEntry, CostRow, TokensSavedEntry};
pub use error::IngotError;
pub use guard::{classify as classify_command, is_safe as command_is_safe, CommandRisk};
pub use handle::IngotHandle;
pub use loop_state::LoopRecord;
pub use mcp::McpServer;
pub use methodology::MethodologyState;
pub use metrics_rollup::{MetricsBucket, RollupTier};
pub use openspec_store::OpenSpecStore;
pub use prompt_hash::PromptHashRecord;
pub use savings_rollup::{SavingsBucket, SavingsSummary};
pub use session::Session;
pub use task::Task;
pub use token_snapshot::TokenSnapshot;

/// Aggregated statistics for a single multi-agent conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationRollup {
    /// Conversation identifier — primary key.
    pub conversation_id: String,
    /// Unix epoch (seconds) when the first event for this conversation was recorded.
    pub started_at: i64,
    /// Unix epoch (seconds) of the most recent event.
    pub last_seen_at: i64,
    /// Number of distinct agents that contributed events.
    pub agent_count: i64,
    /// Total number of LLM-call events (`action_type = "llm"`).
    pub llm_call_count: i64,
    /// Total number of tool-call events (`action_type = "tool"`).
    pub tool_call_count: i64,
    /// Total number of events with `status = "error"`.
    pub failure_count: i64,
    /// Sum of `input_tok` across all events in this conversation.
    pub input_token_total: i64,
    /// Sum of `output_tok` across all events in this conversation.
    pub output_token_total: i64,
}

/// Parses a W3C `traceparent` header into `(trace_id, span_id)`.
///
/// Format: `00-<trace_id>-<parent_id>-<flags>`
///
/// Returns `None` when the input does not conform to the format or the version
/// field is not `"00"`.
#[must_use]
pub fn parse_traceparent(tp: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = tp.splitn(4, '-').collect();
    if parts.len() == 4 && parts[0] == "00" {
        Some((parts[1].to_owned(), parts[2].to_owned()))
    } else {
        None
    }
}

/// `SQLite` persistence handle for smedja.
///
/// Wraps a [`rusqlite::Connection`] and owns all table operations for the
/// smedja data model. On construction the schema is bootstrapped via idempotent
/// `CREATE TABLE IF NOT EXISTS` statements.
pub struct Ingot {
    conn: rusqlite::Connection,
}

impl Ingot {
    /// Opens (or creates) a smedja database file at `path` and runs schema migrations.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the file cannot be opened or migrations fail.
    #[must_use = "the Ingot handle must be used to perform database operations"]
    pub fn open(path: &std::path::Path) -> Result<Self, IngotError> {
        let conn = rusqlite::Connection::open(path)?;
        let ingot = Self { conn };
        ingot.migrate()?;
        Ok(ingot)
    }

    /// Opens an in-memory `SQLite` database and runs schema migrations.
    ///
    /// Useful for tests and ephemeral sessions where durability is not required.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the connection cannot be established or
    /// migrations fail.
    #[must_use = "the Ingot handle must be used to perform database operations"]
    pub fn open_in_memory() -> Result<Self, IngotError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let ingot = Self { conn };
        ingot.migrate()?;
        Ok(ingot)
    }
}

/// Reads a microsecond-count column tolerant of `SQLite` storage class.
///
/// Fresh databases store these columns with `INTEGER` affinity, but databases
/// migrated from the legacy `REAL`-seconds schema retain `REAL` affinity, so a
/// backfilled `CAST(... AS INTEGER)` value is coerced back to a float on write.
/// This helper reads either representation: an integer cell directly, or a real
/// cell rounded to the nearest integer (lossless for realistic micro counts,
/// which stay well below `2^53`).
///
/// # Errors
///
/// Returns the underlying `rusqlite` error if the column holds neither an
/// integer nor a real value.
pub(crate) fn read_micros(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<i64> {
    use rusqlite::types::ValueRef;
    match row.get_ref(idx)? {
        ValueRef::Integer(v) => Ok(v),
        #[allow(clippy::cast_possible_truncation)]
        // rounded micros stay below 2^53 for realistic timestamps/costs
        ValueRef::Real(v) => Ok(v.round() as i64),
        other => Err(rusqlite::Error::InvalidColumnType(
            idx,
            format!("{other:?}"),
            other.data_type(),
        )),
    }
}

/// Returns the current time as a Unix epoch `f64`.
pub(crate) fn now_epoch() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_is_idempotent() {
        let _a = Ingot::open_in_memory().unwrap();
        let _b = Ingot::open_in_memory().unwrap();
    }

    #[test]
    fn parse_traceparent_extracts_ids() {
        let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let (tid, sid) = parse_traceparent(tp).unwrap();
        assert_eq!(tid, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(sid, "00f067aa0ba902b7");
    }

    #[test]
    fn parse_traceparent_rejects_invalid() {
        assert!(parse_traceparent("not-a-traceparent").is_none());
        assert!(parse_traceparent("01-trace-span-01").is_none());
        assert!(parse_traceparent("").is_none());
    }
}
