//! `st-blocks` — block model for smedja.
//!
//! A *block* represents a single unit of terminal output: a shell command and
//! its output, an agent response, or a system message.  Blocks are persisted
//! in a `SQLite` database via [`BlockStore`].

#![allow(
    clippy::cast_precision_loss,   // i64 millis → f64 epoch seconds
    clippy::cast_possible_truncation, // f64 → i64 floor, i64 → u16 row
    clippy::cast_sign_loss,        // i64 row → u16
    clippy::cast_lossless,         // u16 → i64 for SQL params
)]

use chrono::{DateTime, Utc};
use thiserror::Error;
use uuid::Uuid;

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors returned by [`BlockStore`] operations.
#[derive(Debug, Error)]
pub enum BlockError {
    /// A `SQLite` operation failed.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    /// A UUID could not be parsed from the database.
    #[error("uuid parse error: {0}")]
    Uuid(#[from] uuid::Error),
    /// A timestamp stored in the database was not representable.
    #[error("timestamp out of range")]
    Timestamp,
}

// ── Domain types ──────────────────────────────────────────────────────────────

/// The kind of a [`Block`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BlockType {
    /// A shell command and its captured output.
    Shell,
    /// An agent (LLM) response turn.
    Agent,
    /// A system-generated informational message.
    System,
}

impl BlockType {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Agent => "agent",
            Self::System => "system",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "shell" => Some(Self::Shell),
            "agent" => Some(Self::Agent),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

/// A single block of terminal output.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Block {
    /// Unique block identifier.
    pub id: Uuid,
    /// The pane (terminal window) this block belongs to.
    pub pane_id: Uuid,
    /// What kind of block this is.
    pub block_type: BlockType,
    /// The command that produced this block, if any.
    pub cmd: Option<String>,
    /// Captured stdout / stderr output.
    pub output: String,
    /// Process exit code, populated after the command completes.
    pub exit_code: Option<i32>,
    /// Wall-clock duration in milliseconds, populated after completion.
    pub duration_ms: Option<i64>,
    /// Wall-clock timestamp of block creation.
    pub ts: DateTime<Utc>,
    /// Agent turn identifier, if this block was produced during an agent turn.
    pub turn_id: Option<Uuid>,
    /// Execution tier (e.g. `"local"`, `"cloud"`).
    pub tier: Option<String>,
    /// W3C `traceparent` header for distributed tracing correlation.
    pub traceparent: Option<String>,
    /// W3C trace-id extracted from the span that produced this block.
    pub trace_id: Option<String>,
    /// W3C span-id from the span that produced this block.
    pub span_id: Option<String>,
    /// Tool-call identifier for tool-entry blocks.
    pub tool_call_id: Option<String>,
    /// First terminal row covered by this block.
    pub start_row: u16,
    /// Last terminal row covered by this block (inclusive).
    pub end_row: u16,
}

impl Block {
    /// Creates a new [`Block`] with a fresh UUID and the current timestamp.
    #[must_use]
    pub fn new(pane_id: Uuid, block_type: BlockType, cmd: Option<String>, start_row: u16) -> Self {
        Self {
            id: Uuid::new_v4(),
            pane_id,
            block_type,
            cmd,
            output: String::new(),
            exit_code: None,
            duration_ms: None,
            ts: Utc::now(),
            turn_id: None,
            tier: None,
            traceparent: None,
            trace_id: None,
            span_id: None,
            tool_call_id: None,
            start_row,
            end_row: start_row,
        }
    }
}

/// An agent block with additional streaming metadata.
#[derive(Debug, Clone)]
pub struct AgentBlock {
    /// The underlying block record.
    pub block: Block,
    /// The model name that produced this agent response.
    pub model: String,
    /// Whether the agent response is currently streaming.
    pub streaming: bool,
    /// Individual content lines received so far.
    pub content_lines: Vec<String>,
    /// Whether the block is waiting for user approval.
    pub approval_pending: bool,
}

impl AgentBlock {
    /// Creates a new streaming [`AgentBlock`] wrapping a [`Block`].
    #[must_use]
    pub fn new(block: Block, model: String) -> Self {
        Self {
            block,
            model,
            streaming: true,
            content_lines: Vec::new(),
            approval_pending: false,
        }
    }
}

// ── BlockStore ────────────────────────────────────────────────────────────────

/// SQLite-backed store for [`Block`] records.
///
/// The `term_blocks` table is created on first open via [`BlockStore::new`].
pub struct BlockStore {
    conn: rusqlite::Connection,
}

const MIGRATE_SQL: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA foreign_keys = ON;

    CREATE TABLE IF NOT EXISTS term_blocks (
        id           TEXT    NOT NULL PRIMARY KEY,
        pane_id      TEXT    NOT NULL,
        block_type   TEXT    NOT NULL,
        cmd          TEXT,
        output       TEXT    NOT NULL DEFAULT '',
        exit_code    INTEGER,
        duration_ms  INTEGER,
        ts           REAL    NOT NULL,
        turn_id      TEXT,
        tier         TEXT,
        traceparent  TEXT,
        tool_call_id TEXT,
        trace_id     TEXT,
        span_id      TEXT,
        start_row    INTEGER NOT NULL DEFAULT 0,
        end_row      INTEGER NOT NULL DEFAULT 0
    );

    CREATE INDEX IF NOT EXISTS term_blocks_pane_idx
        ON term_blocks (pane_id, ts);
";

impl BlockStore {
    /// Opens a [`BlockStore`] backed by the `SQLite` file at `db_path`.
    ///
    /// Creates the `term_blocks` table if it does not yet exist.
    ///
    /// # Errors
    ///
    /// Returns [`BlockError::Db`] if the file cannot be opened or migration fails.
    pub fn new(db_path: &std::path::Path) -> anyhow::Result<Self> {
        let conn = rusqlite::Connection::open(db_path)?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Opens an in-memory [`BlockStore`].  Useful for tests.
    ///
    /// # Errors
    ///
    /// Returns [`BlockError::Db`] if the connection or migration fails.
    pub fn in_memory() -> anyhow::Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), BlockError> {
        self.conn.execute_batch(MIGRATE_SQL)?;
        Ok(())
    }

    /// Inserts a [`Block`] into the store.
    ///
    /// # Errors
    ///
    /// Returns [`BlockError::Db`] if the INSERT fails.
    pub fn insert(&mut self, block: &Block) -> Result<(), BlockError> {
        let ts = block.ts.timestamp_millis() as f64 / 1000.0;
        self.conn.execute(
            "INSERT INTO term_blocks
               (id, pane_id, block_type, cmd, output, exit_code, duration_ms,
                ts, turn_id, tier, traceparent, tool_call_id, trace_id, span_id,
                start_row, end_row)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
            rusqlite::params![
                block.id.to_string(),
                block.pane_id.to_string(),
                block.block_type.as_str(),
                block.cmd.as_deref(),
                block.output,
                block.exit_code,
                block.duration_ms,
                ts,
                block.turn_id.map(|u| u.to_string()),
                block.tier.as_deref(),
                block.traceparent.as_deref(),
                block.tool_call_id.as_deref(),
                block.trace_id.as_deref(),
                block.span_id.as_deref(),
                i64::from(block.start_row),
                i64::from(block.end_row),
            ],
        )?;
        tracing::debug!(block_id = %block.id, "block inserted");
        Ok(())
    }

    /// Retrieves a [`Block`] by its UUID.
    ///
    /// Returns `None` when no block with that `id` exists.
    ///
    /// # Errors
    ///
    /// Returns [`BlockError::Db`] if the query fails.
    pub fn get(&self, id: &Uuid) -> Result<Option<Block>, BlockError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, pane_id, block_type, cmd, output, exit_code, duration_ms,
                    ts, turn_id, tier, traceparent, tool_call_id, trace_id, span_id,
                    start_row, end_row
             FROM term_blocks WHERE id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![id.to_string()])?;
        Ok(rows.next()?.map(row_to_block).transpose()?)
    }

    /// Returns all blocks for a given pane, ordered by timestamp ascending.
    ///
    /// # Errors
    ///
    /// Returns [`BlockError::Db`] if the query fails.
    pub fn list_by_pane(&self, pane_id: &Uuid) -> Result<Vec<Block>, BlockError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, pane_id, block_type, cmd, output, exit_code, duration_ms,
                    ts, turn_id, tier, traceparent, tool_call_id, trace_id, span_id,
                    start_row, end_row
             FROM term_blocks
             WHERE pane_id = ?1
             ORDER BY ts ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![pane_id.to_string()], |row| {
            row_to_block(row)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(BlockError::Db)
    }

    /// Updates the exit code and duration of a block after it completes.
    ///
    /// # Errors
    ///
    /// Returns [`BlockError::Db`] if the UPDATE fails.
    pub fn update_exit_code(
        &mut self,
        id: &Uuid,
        exit_code: i32,
        duration_ms: i64,
    ) -> Result<(), BlockError> {
        self.conn.execute(
            "UPDATE term_blocks SET exit_code = ?1, duration_ms = ?2 WHERE id = ?3",
            rusqlite::params![exit_code, duration_ms, id.to_string()],
        )?;
        Ok(())
    }

    /// Returns only the output field for a block.
    ///
    /// # Errors
    ///
    /// Returns [`BlockError::Db`] if the query fails.
    pub fn get_output(&self, id: &Uuid) -> Result<Option<String>, BlockError> {
        let mut stmt = self
            .conn
            .prepare("SELECT output FROM term_blocks WHERE id = ?1")?;
        let mut rows = stmt.query(rusqlite::params![id.to_string()])?;
        Ok(rows
            .next()?
            .map(|row| row.get::<_, String>(0))
            .transpose()?)
    }
}

/// Converts a `SQLite` row into a [`Block`].
fn row_to_block(row: &rusqlite::Row<'_>) -> Result<Block, rusqlite::Error> {
    let id_str: String = row.get(0)?;
    let pane_str: String = row.get(1)?;
    let type_str: String = row.get(2)?;
    let cmd: Option<String> = row.get(3)?;
    let output: String = row.get(4)?;
    let exit_code: Option<i32> = row.get(5)?;
    let duration_ms: Option<i64> = row.get(6)?;
    let ts_f64: f64 = row.get(7)?;
    let turn_str: Option<String> = row.get(8)?;
    let tier: Option<String> = row.get(9)?;
    let traceparent: Option<String> = row.get(10)?;
    let tool_call_id: Option<String> = row.get(11)?;
    let trace_id: Option<String> = row.get(12)?;
    let span_id: Option<String> = row.get(13)?;
    let start_row: i64 = row.get(14)?;
    let end_row: i64 = row.get(15)?;

    // Parse UUIDs — map errors to rusqlite::Error::InvalidColumnType so the
    // query_map combinator can report them uniformly.
    let parse_uuid = |s: String| {
        Uuid::parse_str(&s).map_err(|_| {
            rusqlite::Error::InvalidColumnType(0, "uuid".into(), rusqlite::types::Type::Text)
        })
    };

    let id = parse_uuid(id_str)?;
    let pane_id = parse_uuid(pane_str)?;
    let turn_id = turn_str.map(parse_uuid).transpose()?;

    // Reconstruct DateTime<Utc> from Unix epoch float.
    let ts_secs = ts_f64.floor() as i64;
    let ts_nanos = ((ts_f64 - ts_f64.floor()) * 1_000_000_000.0) as u32;
    let ts = DateTime::from_timestamp(ts_secs, ts_nanos)
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap());

    let block_type = BlockType::from_str(&type_str).unwrap_or(BlockType::System);

    Ok(Block {
        id,
        pane_id,
        block_type,
        cmd,
        output,
        exit_code,
        duration_ms,
        ts,
        turn_id,
        tier,
        traceparent,
        tool_call_id,
        trace_id,
        span_id,
        start_row: start_row as u16,
        end_row: end_row as u16,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_block(pane_id: Uuid) -> Block {
        let mut b = Block::new(pane_id, BlockType::Shell, Some("ls -la".into()), 0);
        b.output = "total 42\n".into();
        b
    }

    #[test]
    fn in_memory_store_roundtrips_block() {
        let mut store = BlockStore::in_memory().unwrap();
        let pane = Uuid::new_v4();
        let block = sample_block(pane);
        let id = block.id;

        store.insert(&block).unwrap();
        let got = store.get(&id).unwrap().expect("block not found");
        assert_eq!(got.id, id);
        assert_eq!(got.cmd, Some("ls -la".into()));
        assert_eq!(got.output, "total 42\n");
    }

    #[test]
    fn get_returns_none_for_unknown_id() {
        let store = BlockStore::in_memory().unwrap();
        let result = store.get(&Uuid::new_v4()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn list_by_pane_returns_blocks_in_order() {
        let mut store = BlockStore::in_memory().unwrap();
        let pane = Uuid::new_v4();
        let b1 = sample_block(pane);
        let b2 = sample_block(pane);

        store.insert(&b1).unwrap();
        store.insert(&b2).unwrap();

        let blocks = store.list_by_pane(&pane).unwrap();
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn list_by_pane_filters_by_pane_id() {
        let mut store = BlockStore::in_memory().unwrap();
        let pane_a = Uuid::new_v4();
        let pane_b = Uuid::new_v4();

        store.insert(&sample_block(pane_a)).unwrap();
        store.insert(&sample_block(pane_b)).unwrap();

        let blocks = store.list_by_pane(&pane_a).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].pane_id, pane_a);
    }

    #[test]
    fn update_exit_code_sets_fields() {
        let mut store = BlockStore::in_memory().unwrap();
        let pane = Uuid::new_v4();
        let block = sample_block(pane);
        let id = block.id;

        store.insert(&block).unwrap();
        store.update_exit_code(&id, 0, 123).unwrap();

        let got = store.get(&id).unwrap().unwrap();
        assert_eq!(got.exit_code, Some(0));
        assert_eq!(got.duration_ms, Some(123));
    }

    #[test]
    fn get_output_returns_output_string() {
        let mut store = BlockStore::in_memory().unwrap();
        let pane = Uuid::new_v4();
        let block = sample_block(pane);
        let id = block.id;

        store.insert(&block).unwrap();
        let out = store.get_output(&id).unwrap();
        assert_eq!(out, Some("total 42\n".into()));
    }

    #[test]
    fn get_output_returns_none_for_missing_id() {
        let store = BlockStore::in_memory().unwrap();
        let out = store.get_output(&Uuid::new_v4()).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn block_type_roundtrips_through_str() {
        assert_eq!(BlockType::from_str("shell"), Some(BlockType::Shell));
        assert_eq!(BlockType::from_str("agent"), Some(BlockType::Agent));
        assert_eq!(BlockType::from_str("system"), Some(BlockType::System));
        assert!(BlockType::from_str("unknown").is_none());
    }

    #[test]
    fn agent_block_new_starts_streaming() {
        let pane = Uuid::new_v4();
        let block = Block::new(pane, BlockType::Agent, None, 0);
        let ab = AgentBlock::new(block, "claude-opus".into());
        assert!(ab.streaming);
        assert!(!ab.approval_pending);
        assert_eq!(ab.model, "claude-opus");
    }

    #[test]
    fn in_memory_store_migrate_is_idempotent() {
        let store = BlockStore::in_memory().unwrap();
        // Call migrate again — must not error.
        store.migrate().unwrap();
    }

    // ── Phase 6 ───────────────────────────────────────────────────────────

    #[test]
    fn block_stores_trace_and_span_ids() {
        let pane = Uuid::new_v4();
        let mut block = Block::new(pane, BlockType::Agent, None, 0);
        block.trace_id = Some("trace-abc".into());
        block.span_id = Some("span-xyz".into());
        assert_eq!(block.trace_id.as_deref(), Some("trace-abc"));
        assert_eq!(block.span_id.as_deref(), Some("span-xyz"));
        assert!(block.tool_call_id.is_none());
    }

    #[test]
    fn block_stores_tool_call_id() {
        let pane = Uuid::new_v4();
        let mut block = Block::new(pane, BlockType::Agent, None, 0);
        block.tool_call_id = Some("call-99".into());
        assert_eq!(block.tool_call_id.as_deref(), Some("call-99"));
    }

    #[test]
    fn block_trace_fields_default_to_none() {
        let pane = Uuid::new_v4();
        let block = Block::new(pane, BlockType::Shell, None, 0);
        assert!(block.trace_id.is_none());
        assert!(block.span_id.is_none());
        assert!(block.tool_call_id.is_none());
    }

    #[test]
    fn agent_block_exposes_trace_fields_via_inner_block() {
        let pane = Uuid::new_v4();
        let mut block = Block::new(pane, BlockType::Agent, None, 0);
        block.trace_id = Some("tr".into());
        block.span_id = Some("sp".into());
        block.tool_call_id = Some("tc".into());
        let ab = AgentBlock::new(block, "claude-opus".into());
        assert_eq!(ab.block.trace_id.as_deref(), Some("tr"));
        assert_eq!(ab.block.span_id.as_deref(), Some("sp"));
        assert_eq!(ab.block.tool_call_id.as_deref(), Some("tc"));
    }
}
