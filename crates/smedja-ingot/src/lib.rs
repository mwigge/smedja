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
pub mod openspec_store;
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

/// The current schema version recorded in the legacy `schema_version` marker
/// table. Derived from the number of numbered [`MIGRATIONS`] so it can never
/// drift from the migrations actually defined.
#[allow(clippy::cast_possible_wrap)] // MIGRATIONS.len() is a small compile-time constant
const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Numbered migrations applied in sequence after the base DDL.
///
/// Each entry is `(version, sql)`. The `sql` may be a single statement or a
/// semicolon-separated batch. Migrations are applied in ascending version order
/// and recorded in `schema_migrations` so they are never applied twice.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, "ALTER TABLE tasks ADD COLUMN response TEXT;"),
    (
        2,
        "ALTER TABLE sessions ADD COLUMN cowork_mode INTEGER NOT NULL DEFAULT 0;",
    ),
    (3, "ALTER TABLE sessions ADD COLUMN workspace_root TEXT;"),
    (4, "ALTER TABLE sessions ADD COLUMN model_override TEXT;"),
    (5, "ALTER TABLE sessions ADD COLUMN runner_override TEXT;"),
    (6, "ALTER TABLE audit_events ADD COLUMN role_id TEXT;"),
    (
        7,
        "ALTER TABLE audit_events ADD COLUMN conversation_id TEXT;",
    ),
    (8, "ALTER TABLE audit_events ADD COLUMN trace_id TEXT;"),
    (9, "ALTER TABLE audit_events ADD COLUMN span_id TEXT;"),
    (
        10,
        "ALTER TABLE audit_events ADD COLUMN parent_span_id TEXT;",
    ),
    (11, "ALTER TABLE audit_events ADD COLUMN agent_name TEXT;"),
    (
        12,
        "ALTER TABLE audit_events ADD COLUMN operation_name TEXT;",
    ),
    (13, "ALTER TABLE audit_events ADD COLUMN status TEXT;"),
    (14, "ALTER TABLE audit_events ADD COLUMN error_kind TEXT;"),
    (
        15,
        "ALTER TABLE audit_events ADD COLUMN error_count INTEGER;",
    ),
    (16, "ALTER TABLE audit_events ADD COLUMN tool_call_id TEXT;"),
    (
        17,
        "ALTER TABLE sessions ADD COLUMN title TEXT NOT NULL DEFAULT '';",
    ),
    (
        18,
        "CREATE TABLE IF NOT EXISTS session_methodology ( \
            session_id TEXT PRIMARY KEY, \
            spec_recorded INTEGER NOT NULL DEFAULT 0, \
            approval_recorded INTEGER NOT NULL DEFAULT 0, \
            no_spec_gate INTEGER NOT NULL DEFAULT 0 \
         );",
    ),
    // Timestamp backfill: convert legacy REAL-seconds columns to INTEGER
    // microseconds. SQLite columns have dynamic type affinity, so the same
    // column can hold both before and after this one-time, version-gated
    // conversion. On fresh databases every table is empty (or already stores
    // micros), so these UPDATEs are no-ops and stay idempotent.
    (
        19,
        "UPDATE audit_events          SET ts         = CAST(round(ts * 1000000) AS INTEGER); \
         UPDATE sessions              SET created_at = CAST(round(created_at * 1000000) AS INTEGER), \
                                          updated_at = CAST(round(updated_at * 1000000) AS INTEGER); \
         UPDATE tasks                 SET created_at = CAST(round(created_at * 1000000) AS INTEGER); \
         UPDATE checkpoints           SET created_at = CAST(round(created_at * 1000000) AS INTEGER); \
         UPDATE cost_ledger           SET created_at = CAST(round(created_at * 1000000) AS INTEGER); \
         UPDATE loops                 SET created_at = CAST(round(created_at * 1000000) AS INTEGER), \
                                          updated_at = CAST(round(updated_at * 1000000) AS INTEGER); \
         UPDATE turn_token_snapshots  SET created_at = CAST(round(created_at * 1000000) AS INTEGER);",
    ),
    // Money backfill: convert legacy REAL-dollar cost_ledger.cost_usd to
    // INTEGER microdollars. Version-gated so it runs exactly once.
    (
        20,
        "UPDATE cost_ledger SET cost_usd = CAST(round(cost_usd * 1000000) AS INTEGER);",
    ),
    // Compaction redesign: add an explicit compaction_id discriminator and a
    // partial unique index, and migrate legacy turn_n = -1 sentinel rows onto a
    // generated compaction_id so multiple compactions per session are retained.
    //
    // Pre-existing databases declared a table-level UNIQUE(session_id, turn_n)
    // that would still collide on the -1 sentinel, so the table is rebuilt to
    // drop that constraint. The leading DROP clears any partially-rebuilt table
    // from an interrupted prior attempt, keeping this re-runnable.
    (
        21,
        "DROP TABLE IF EXISTS checkpoints_old; \
         ALTER TABLE checkpoints RENAME TO checkpoints_old; \
         DROP INDEX IF EXISTS idx_checkpoints_turn; \
         CREATE TABLE checkpoints ( \
             id            TEXT PRIMARY KEY, \
             session_id    TEXT NOT NULL, \
             turn_n        INTEGER NOT NULL, \
             messages_json TEXT NOT NULL, \
             created_at    INTEGER NOT NULL, \
             compaction_id TEXT \
         ); \
         CREATE UNIQUE INDEX IF NOT EXISTS idx_checkpoints_turn \
             ON checkpoints(session_id, turn_n) WHERE compaction_id IS NULL; \
         INSERT INTO checkpoints (id, session_id, turn_n, messages_json, created_at, compaction_id) \
             SELECT id, session_id, turn_n, messages_json, created_at, \
                    CASE WHEN turn_n = -1 THEN 'legacy-compaction-' || id ELSE NULL END \
             FROM checkpoints_old; \
         DROP TABLE checkpoints_old;",
    ),
    // Metrics rollups: a derived cache for time-tiered aggregation of tokens,
    // cost, turns, and error counts per runner. Keyed on (tier, bucket_start,
    // runner). Created via IF NOT EXISTS so the migration is idempotent. The two
    // indices accelerate the on-read aggregation over the source tables, which
    // bound every query by created_at / ts.
    (
        22,
        "CREATE TABLE IF NOT EXISTS metrics_rollups ( \
             tier         TEXT NOT NULL, \
             bucket_start INTEGER NOT NULL, \
             runner       TEXT NOT NULL, \
             turns        INTEGER NOT NULL DEFAULT 0, \
             input_tok    INTEGER NOT NULL DEFAULT 0, \
             output_tok   INTEGER NOT NULL DEFAULT 0, \
             cost_usd     INTEGER NOT NULL DEFAULT 0, \
             error_count  INTEGER NOT NULL DEFAULT 0, \
             PRIMARY KEY (tier, bucket_start, runner) \
         ); \
         CREATE INDEX IF NOT EXISTS idx_cost_ledger_created_at \
             ON cost_ledger(created_at); \
         CREATE INDEX IF NOT EXISTS idx_audit_events_ts_status \
             ON audit_events(ts, status);",
    ),
    // Output-filter savings ledger: tokens saved by command-output filtering,
    // recorded separately from billed cost_ledger totals so smj cost can
    // attribute filtering value without polluting the exact billed input/output
    // token sums. Created via IF NOT EXISTS so the migration is idempotent.
    (
        23,
        "CREATE TABLE IF NOT EXISTS tokens_saved_ledger ( \
             id           TEXT PRIMARY KEY, \
             session_id   TEXT NOT NULL, \
             turn_n       INTEGER NOT NULL, \
             command      TEXT NOT NULL, \
             tokens_saved INTEGER NOT NULL DEFAULT 0, \
             created_at   INTEGER NOT NULL \
         ); \
         CREATE INDEX IF NOT EXISTS idx_tokens_saved_session \
             ON tokens_saved_ledger(session_id);",
    ),
    // Multi-source savings ledger: add a `source` discriminator so every token
    // saver (filter, crusher, cold-context, cache, lean-spec, …) writes a tagged
    // row to one ledger. Existing rows are all written by the output-filter path,
    // so the DEFAULT 'filter' backfills them with the historically-correct value.
    // The defaulted ADD COLUMN and the IF NOT EXISTS index keep this re-runnable,
    // matching migrations 22/23. The source index accelerates per-source rollups.
    (
        24,
        "ALTER TABLE tokens_saved_ledger ADD COLUMN source TEXT NOT NULL DEFAULT 'filter'; \
         CREATE INDEX IF NOT EXISTS idx_tokens_saved_source \
             ON tokens_saved_ledger(source);",
    ),
    // Value panel: attribute token cost to the active openspec change so the
    // value panel can show cumulative burn per change. NULL on rows written
    // before this migration (no change was active or smdjad was not updated).
    (
        25,
        "ALTER TABLE audit_events ADD COLUMN change_name TEXT;",
    ),
];

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

    /// Applies all `CREATE TABLE IF NOT EXISTS` statements, making schema bootstrap
    /// fully idempotent.
    #[allow(clippy::too_many_lines)] // DDL — length is inherent, not complexity
    fn migrate(&self) -> Result<(), IngotError> {
        self.conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous  = NORMAL;
            PRAGMA busy_timeout = 5000;
            -- NOTE: foreign_keys is intentionally left at its default (OFF).
            -- This schema declares no REFERENCES clauses; referential integrity
            -- between sessions/tasks/checkpoints/cost_ledger is enforced in
            -- application code, not by SQLite. The previous `PRAGMA foreign_keys
            -- = ON` was misleading (no FKs to enforce) and has been removed.

            CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY
            );

            CREATE TABLE IF NOT EXISTS schema_migrations (
                version    INTEGER PRIMARY KEY,
                applied_at REAL NOT NULL
            );

            -- Timestamp columns store INTEGER microseconds since the Unix epoch
            -- (smedja_types::Timestamp). Monetary columns store INTEGER
            -- microdollars (smedja_types::Microdollars). Pre-existing databases
            -- that stored REAL seconds / REAL dollars are converted by the
            -- version-gated backfill migrations below.
            CREATE TABLE IF NOT EXISTS audit_events (
                id          TEXT PRIMARY KEY,
                ts          INTEGER NOT NULL,
                session_id  TEXT NOT NULL,
                turn_id     TEXT,
                action_type TEXT NOT NULL,
                actor       TEXT NOT NULL,
                tool_name   TEXT,
                input_tok   INTEGER NOT NULL DEFAULT 0,
                output_tok  INTEGER NOT NULL DEFAULT 0,
                latency_ms  INTEGER NOT NULL DEFAULT 0,
                traceparent TEXT,
                tier        TEXT
            );

            CREATE TABLE IF NOT EXISTS sessions (
                id             TEXT PRIMARY KEY,
                created_at     INTEGER NOT NULL,
                updated_at     INTEGER NOT NULL,
                status         TEXT NOT NULL DEFAULT 'active',
                task_id        TEXT,
                mode           TEXT,
                cowork_mode    INTEGER NOT NULL DEFAULT 0,
                workspace_root TEXT,
                model_override TEXT
            );

            CREATE TABLE IF NOT EXISTS mcp_servers (
                id           TEXT PRIMARY KEY,
                name         TEXT NOT NULL,
                url          TEXT NOT NULL DEFAULT '',
                transport    TEXT NOT NULL DEFAULT 'http',
                tools_json   TEXT NOT NULL DEFAULT '[]',
                last_refresh REAL NOT NULL DEFAULT 0.0
            );

            CREATE TABLE IF NOT EXISTS tasks (
                id          TEXT PRIMARY KEY,
                title       TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                status      TEXT NOT NULL DEFAULT 'planned',
                created_at  INTEGER NOT NULL,
                session_id  TEXT,
                response    TEXT
            );

            -- Fresh databases get the new checkpoints shape here; the partial
            -- unique index (and, for pre-existing databases, the rebuild that
            -- drops the legacy table-level UNIQUE(session_id, turn_n)) is applied
            -- by the version-gated compaction migration below. Defining the index
            -- here would fail on old databases whose checkpoints table predates
            -- the compaction_id column.
            CREATE TABLE IF NOT EXISTS checkpoints (
                id            TEXT PRIMARY KEY,
                session_id    TEXT NOT NULL,
                turn_n        INTEGER NOT NULL,
                messages_json TEXT NOT NULL,
                created_at    INTEGER NOT NULL,
                compaction_id TEXT
            );

            CREATE TABLE IF NOT EXISTS cost_ledger (
                id          TEXT PRIMARY KEY,
                session_id  TEXT NOT NULL,
                turn_n      INTEGER NOT NULL,
                runner      TEXT NOT NULL,
                model       TEXT NOT NULL,
                input_tok   INTEGER NOT NULL DEFAULT 0,
                output_tok  INTEGER NOT NULL DEFAULT 0,
                cost_usd    INTEGER NOT NULL DEFAULT 0,
                created_at  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS loops (
                id            TEXT PRIMARY KEY,
                change_name   TEXT NOT NULL,
                status        TEXT NOT NULL DEFAULT 'planning',
                current_slice INTEGER NOT NULL DEFAULT 0,
                attempt       INTEGER NOT NULL DEFAULT 1,
                created_at    INTEGER NOT NULL,
                updated_at    INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS turn_token_snapshots (
                id               TEXT PRIMARY KEY,
                session_id       TEXT NOT NULL,
                turn_n           INTEGER NOT NULL,
                input_tok        INTEGER NOT NULL DEFAULT 0,
                output_tok       INTEGER NOT NULL DEFAULT 0,
                cumulative_input INTEGER NOT NULL DEFAULT 0,
                cumulative_output INTEGER NOT NULL DEFAULT 0,
                created_at       INTEGER NOT NULL,
                UNIQUE(session_id, turn_n)
            );

            CREATE TABLE IF NOT EXISTS prompt_hashes (
                id          TEXT PRIMARY KEY,
                change_name TEXT NOT NULL,
                role        TEXT NOT NULL,
                hash        TEXT NOT NULL,
                ts          REAL NOT NULL
            );
            ",
        )?;

        // conversation_rollups table (created unconditionally via IF NOT EXISTS).
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS conversation_rollups (
                conversation_id    TEXT PRIMARY KEY,
                started_at         INTEGER NOT NULL,
                last_seen_at       INTEGER NOT NULL,
                agent_count        INTEGER NOT NULL DEFAULT 0,
                llm_call_count     INTEGER NOT NULL DEFAULT 0,
                tool_call_count    INTEGER NOT NULL DEFAULT 0,
                failure_count      INTEGER NOT NULL DEFAULT 0,
                input_token_total  INTEGER NOT NULL DEFAULT 0,
                output_token_total INTEGER NOT NULL DEFAULT 0
            );",
        )?;

        // Version-gated incremental migrations.
        //
        // Read the highest version already applied, then run each entry in
        // MIGRATIONS whose version number exceeds that high-water mark.  After
        // each successful statement we record the version in schema_migrations so
        // the migration is never applied again — even if migrate() is called a
        // second time on the same connection (idempotency is preserved).
        let applied: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let now = now_epoch();
        for &(version, sql) in MIGRATIONS {
            if version <= applied {
                continue;
            }
            // Each ALTER TABLE may fail on a fresh database where the column was
            // already present in the base CREATE TABLE DDL above.  Run the DDL
            // inside a savepoint so idempotent errors (duplicate column, table
            // already exists) are silently ignored, but real failures are
            // propagated rather than silently recording the migration as applied.
            let sp_name = format!("migration_{version}");
            self.conn
                .execute_batch(&format!("SAVEPOINT \"{sp_name}\""))?;
            let ddl_result = self.conn.execute_batch(sql);
            match ddl_result {
                Ok(()) => {
                    self.conn.execute_batch(&format!("RELEASE \"{sp_name}\""))?;
                }
                Err(ref e) if is_idempotent_ddl_error(e) => {
                    self.conn.execute_batch(&format!("RELEASE \"{sp_name}\""))?;
                }
                Err(e) => {
                    self.conn
                        .execute_batch(&format!("ROLLBACK TO \"{sp_name}\""))?;
                    self.conn.execute_batch(&format!("RELEASE \"{sp_name}\""))?;
                    return Err(IngotError::Db(e));
                }
            }
            self.conn.execute(
                "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                rusqlite::params![version, now],
            )?;
        }

        // Record (or ignore) the legacy schema version marker.
        self.conn.execute(
            "INSERT OR IGNORE INTO schema_version (version) VALUES (?1)",
            rusqlite::params![SCHEMA_VERSION],
        )?;

        Ok(())
    }

    // audit_events -----------------------------------------------------------

    /// Appends an [`AuditEvent`] to the immutable audit log.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails.
    #[must_use = "check the Result to confirm the event was persisted"]
    pub fn insert_audit_event(&self, event: &AuditEvent) -> Result<(), IngotError> {
        audit::insert(&self.conn, event)
    }

    /// Returns all [`AuditEvent`]s for `session_id`, ordered by `ts` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned events"]
    pub fn list_audit_events(&self, session_id: &str) -> Result<Vec<AuditEvent>, IngotError> {
        audit::list_by_session(&self.conn, session_id)
    }

    /// Returns the sum of `input_tok + output_tok` across all audit events with
    /// the given `change_name`. Returns `Ok(0)` when no matching rows exist or when
    /// the `change_name` column is absent on a pre-migration database.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned token total"]
    pub fn cost_for_change(&self, change_name: &str) -> Result<u64, IngotError> {
        let total: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(input_tok + output_tok), 0) \
             FROM audit_events WHERE change_name = ?1",
            rusqlite::params![change_name],
            |row| row.get(0),
        )?;
        Ok(total.max(0).cast_unsigned())
    }

    /// Persists a timeline event and, when the event carries a `conversation_id`,
    /// upserts the matching [`ConversationRollup`] counters atomically.
    ///
    /// - `llm_call_count` incremented when `action_type == "llm"`
    /// - `tool_call_count` incremented when `action_type == "tool"`
    /// - `failure_count` incremented when `status == Some("error")`
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if either the event INSERT or the rollup upsert fails.
    #[must_use = "check the Result to confirm the timeline event was recorded"]
    pub fn record_timeline_event(&self, event: &AuditEvent) -> Result<(), IngotError> {
        audit::insert(&self.conn, event)?;

        let Some(ref conv_id) = event.conversation_id else {
            return Ok(());
        };

        #[allow(clippy::cast_possible_truncation)] // intentional: subsecond precision is not needed
        let now_secs = now_epoch() as i64;
        let is_llm = i64::from(event.action_type == "llm");
        let is_tool = i64::from(event.action_type == "tool");
        let is_failure = i64::from(event.status.as_deref() == Some("error"));

        self.conn.execute(
            "INSERT INTO conversation_rollups \
             (conversation_id, started_at, last_seen_at, agent_count, \
              llm_call_count, tool_call_count, failure_count, \
              input_token_total, output_token_total) \
             VALUES (?1, ?2, ?2, 0, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(conversation_id) DO UPDATE SET \
               last_seen_at       = excluded.last_seen_at, \
               agent_count        = (SELECT COUNT(DISTINCT COALESCE(agent_name, actor)) \
                                     FROM audit_events WHERE conversation_id = ?1), \
               llm_call_count     = llm_call_count     + excluded.llm_call_count, \
               tool_call_count    = tool_call_count    + excluded.tool_call_count, \
               failure_count      = failure_count      + excluded.failure_count, \
               input_token_total  = input_token_total  + excluded.input_token_total, \
               output_token_total = output_token_total + excluded.output_token_total",
            rusqlite::params![
                conv_id,
                now_secs,
                is_llm,
                is_tool,
                is_failure,
                event.input_tok,
                event.output_tok,
            ],
        )?;
        Ok(())
    }

    /// Returns the most recent `limit` [`ConversationRollup`]s ordered by
    /// `last_seen_at` descending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned rollups"]
    pub fn recent_conversations(&self, limit: u32) -> Result<Vec<ConversationRollup>, IngotError> {
        let mut stmt = self.conn.prepare(
            "SELECT conversation_id, started_at, last_seen_at, agent_count, \
                    llm_call_count, tool_call_count, failure_count, \
                    input_token_total, output_token_total \
             FROM conversation_rollups \
             ORDER BY last_seen_at DESC \
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit], |row| {
            Ok(ConversationRollup {
                conversation_id: row.get(0)?,
                started_at: row.get(1)?,
                last_seen_at: row.get(2)?,
                agent_count: row.get(3)?,
                llm_call_count: row.get(4)?,
                tool_call_count: row.get(5)?,
                failure_count: row.get(6)?,
                input_token_total: row.get(7)?,
                output_token_total: row.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(IngotError::Db)
    }

    /// Returns all timeline events for `conversation_id`, ordered by `rowid` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned events"]
    pub fn conversation_timeline(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<AuditEvent>, IngotError> {
        audit::list_by_conversation(&self.conn, conversation_id)
    }

    /// Returns timeline events with `status = 'error'` for `conversation_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned events"]
    pub fn failed_events(&self, conversation_id: &str) -> Result<Vec<AuditEvent>, IngotError> {
        audit::list_failed_by_conversation(&self.conn, conversation_id)
    }

    // sessions ---------------------------------------------------------------

    /// Inserts a new [`Session`].
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails.
    #[must_use = "check the Result to confirm the session was created"]
    pub fn create_session(&self, session: &Session) -> Result<(), IngotError> {
        session::create(&self.conn, session)
    }

    /// Retrieves a [`Session`] by `id`, returning `None` when not found.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned session"]
    pub fn get_session(&self, id: &str) -> Result<Option<Session>, IngotError> {
        session::get(&self.conn, id)
    }

    /// Returns all [`Session`]s ordered by `created_at` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned sessions"]
    pub fn list_sessions(&self) -> Result<Vec<Session>, IngotError> {
        session::list(&self.conn)
    }

    /// Searches sessions where `title` or `workspace_root` contains `query` (case-insensitive).
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the matched sessions"]
    pub fn search_sessions(&self, query: &str) -> Result<Vec<Session>, IngotError> {
        session::search(&self.conn, query)
    }

    /// Deletes the session with the given `id`.
    ///
    /// Returns `true` if a row was deleted, `false` if no session with that `id`
    /// existed.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the DELETE fails.
    #[must_use = "check the Result to confirm the session was deleted"]
    pub fn delete_session(&self, id: &str) -> Result<bool, IngotError> {
        session::delete(&self.conn, id)
    }

    /// Deletes sessions with a terminal status (`complete`, `failed`, `orphaned`)
    /// whose `updated_at` timestamp is older than `older_than_days` days, then
    /// removes orphaned dependent rows from `checkpoints`, `cost_ledger`,
    /// `audit_events`, and `tasks`. Returns the number of sessions deleted.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError`] on database failure.
    pub fn prune_old_sessions(&self, older_than_days: u64) -> Result<usize, IngotError> {
        let micros_per_day: i64 = 86_400 * 1_000_000;
        let cutoff = smedja_types::Timestamp::now().as_micros()
            - i64::try_from(older_than_days).unwrap_or(i64::MAX) * micros_per_day;

        let deleted = {
            let tx = self.conn.unchecked_transaction()?;
            let n = tx.execute(
                "DELETE FROM sessions WHERE status IN ('complete','failed','orphaned') AND updated_at < ?1",
                rusqlite::params![cutoff],
            )?;
            for table in &["checkpoints", "cost_ledger", "audit_events"] {
                tx.execute(
                    &format!(
                        "DELETE FROM {table} WHERE session_id NOT IN (SELECT id FROM sessions)"
                    ),
                    [],
                )?;
            }
            tx.execute(
                "DELETE FROM tasks WHERE session_id IS NOT NULL AND session_id NOT IN (SELECT id FROM sessions)",
                [],
            )?;
            tx.commit()?;
            n
        };

        Ok(deleted)
    }

    /// Checkpoints the WAL and rebuilds the database file to reclaim space.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError`] on database failure.
    pub fn vacuum(&self) -> Result<(), IngotError> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        Ok(())
    }

    /// Updates the `status` of a session to `status` and records a new `updated_at`
    /// timestamp using the current Unix epoch.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the status was updated"]
    pub fn update_session_status(&self, id: &str, status: &str) -> Result<(), IngotError> {
        session::update_status(&self.conn, id, status, smedja_types::Timestamp::now())
    }

    /// Sets the `workspace_root` filesystem path for the session identified by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the workspace root was updated"]
    pub fn update_session_workspace_root(
        &self,
        session_id: &str,
        workspace_root: &str,
    ) -> Result<(), IngotError> {
        session::update_workspace_root(&self.conn, session_id, workspace_root)
    }

    /// Sets the `mode` field for the session identified by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the mode was updated"]
    pub fn update_session_mode(&self, session_id: &str, mode: &str) -> Result<(), IngotError> {
        session::update_mode(&self.conn, session_id, mode)
    }

    /// Sets the `model_override` field for the session identified by `session_id`.
    ///
    /// When set, `run_turn` uses this model name instead of the `SMEDJA_MODEL`
    /// environment variable.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the model override was updated"]
    pub fn update_session_model_override(
        &self,
        session_id: &str,
        model: &str,
    ) -> Result<(), IngotError> {
        session::update_model_override(&self.conn, session_id, model).map_err(IngotError::Db)
    }

    /// Sets the `runner_override` field for the session identified by `session_id`.
    ///
    /// When set, `run_turn` bypasses the assayer and routes directly to this runner
    /// (e.g. `"claude-cli"`, `"codex-cli"`, `"local"`).
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the runner override was updated"]
    pub fn update_session_runner_override(
        &self,
        session_id: &str,
        runner: &str,
    ) -> Result<(), IngotError> {
        session::update_runner_override(&self.conn, session_id, runner).map_err(IngotError::Db)
    }

    /// Links the session identified by `session_id` to a task by setting `task_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the task id was linked"]
    pub fn update_session_task_id(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<(), IngotError> {
        session::update_task_id(&self.conn, session_id, task_id)
    }

    /// Enables or disables the cowork gate for the session identified by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the cowork mode was updated"]
    pub fn update_session_cowork_mode(
        &self,
        session_id: &str,
        enabled: bool,
    ) -> Result<(), IngotError> {
        session::update_cowork_mode(&self.conn, session_id, enabled)
    }

    /// Sets the human-readable `title` for the session identified by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    pub fn update_session_title(&self, session_id: &str, title: &str) -> Result<(), IngotError> {
        session::update_title(&self.conn, session_id, title)
    }

    // mcp_servers ------------------------------------------------------------

    /// Registers (or replaces) an [`McpServer`] in the registry.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT OR REPLACE fails.
    #[must_use = "check the Result to confirm the MCP server was registered"]
    pub fn register_mcp_server(&self, server: &McpServer) -> Result<(), IngotError> {
        mcp::insert(&self.conn, server)
    }

    /// Returns all registered [`McpServer`]s ordered by `name` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned servers"]
    pub fn list_mcp_servers(&self) -> Result<Vec<McpServer>, IngotError> {
        mcp::list(&self.conn)
    }

    /// Removes the [`McpServer`] with the given `id` from the registry.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the DELETE fails.
    #[must_use = "check the Result to confirm the MCP server was removed"]
    pub fn remove_mcp_server(&self, id: &str) -> Result<(), IngotError> {
        mcp::remove(&self.conn, id)
    }

    /// Updates the cached tool list and refresh timestamp for the server identified
    /// by `name`. Sets `last_refresh` to the current Unix epoch.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the tool list was updated"]
    pub fn update_mcp_tools(&self, name: &str, tools_json: &str) -> Result<(), IngotError> {
        mcp::update_tools(&self.conn, name, tools_json)
    }

    /// Returns all registered [`McpServer`]s whose `last_refresh` is older than
    /// `older_than_secs` seconds ago, or that have never been refreshed.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned servers"]
    pub fn get_stale_servers(&self, older_than_secs: f64) -> Result<Vec<McpServer>, IngotError> {
        mcp::stale(&self.conn, older_than_secs)
    }

    /// Returns all MCP servers that have a non-empty `tools_json`, as
    /// `(server_name, tools_json)` pairs.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned tool pairs"]
    pub fn get_all_mcp_tools(&self) -> Result<Vec<(String, String)>, IngotError> {
        mcp::all_tools(&self.conn)
    }

    /// Looks up a single [`McpServer`] by its registered name, returning `None`
    /// when no server with that name exists.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned server"]
    pub fn get_mcp_server_by_name(&self, name: &str) -> Result<Option<McpServer>, IngotError> {
        mcp::by_name(&self.conn, name)
    }

    /// Finds the MCP server that exposes a tool with the given `tool_name`.
    ///
    /// Searches the `tools_json` of every server that has a non-empty tool
    /// list.  Returns the first server whose list contains a tool entry with
    /// `"name": tool_name`, or `None` when no match is found.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the tool-list query fails.
    #[must_use = "check the Result; None means no registered MCP server owns this tool"]
    pub fn find_mcp_server_for_tool(
        &self,
        tool_name: &str,
    ) -> Result<Option<McpServer>, IngotError> {
        for (server_name, tools_json) in mcp::all_tools(&self.conn)? {
            let tools: Vec<serde_json::Value> =
                serde_json::from_str(&tools_json).unwrap_or_default();
            let owns_tool = tools
                .iter()
                .any(|t| t.get("name").and_then(|v| v.as_str()) == Some(tool_name));
            if owns_tool {
                return mcp::by_name(&self.conn, &server_name);
            }
        }
        Ok(None)
    }

    // tasks ------------------------------------------------------------------

    /// Inserts a new [`Task`].
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails.
    #[must_use = "check the Result to confirm the task was created"]
    pub fn create_task(&self, task: &Task) -> Result<(), IngotError> {
        task::create(&self.conn, task)
    }

    /// Returns tasks, optionally filtered by `status`.
    ///
    /// Pass `None` to return all tasks. Pass `Some("planned")` (or any other valid
    /// status string) to restrict the result set.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned tasks"]
    pub fn list_tasks(&self, status: Option<&str>) -> Result<Vec<Task>, IngotError> {
        task::list(&self.conn, status)
    }

    /// Returns the completed conversation turns for `session_id`, oldest first
    /// (each task's `title` is the user message, `response` the assistant reply).
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    pub fn session_history(&self, session_id: &str) -> Result<Vec<Task>, IngotError> {
        task::history_for_session(&self.conn, session_id)
    }

    /// Updates the `status` field for the task identified by `id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the status was updated"]
    pub fn update_task_status(&self, id: &str, status: &str) -> Result<(), IngotError> {
        task::update_status(&self.conn, id, status)
    }

    /// Retrieves a [`Task`] by `id`, returning `None` when not found.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned task"]
    pub fn get_task(&self, id: &str) -> Result<Option<Task>, IngotError> {
        task::get(&self.conn, id)
    }

    /// Stores `response` text for the task identified by `id` and sets
    /// `status = "complete"` in the same UPDATE.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the response was stored"]
    pub fn set_task_response(&self, id: &str, response: &str) -> Result<(), IngotError> {
        task::update_response(&self.conn, id, response)
    }

    // checkpoints ------------------------------------------------------------

    /// Saves a [`Checkpoint`], replacing any existing checkpoint for the same
    /// `(session_id, turn_n)` pair.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the upsert fails.
    #[must_use = "check the Result to confirm the checkpoint was saved"]
    pub fn save_checkpoint(&self, cp: &Checkpoint) -> Result<(), IngotError> {
        checkpoint::save(&self.conn, cp)
    }

    /// Loads the [`Checkpoint`] for `(session_id, turn_n)`, returning `None` if not found.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned checkpoint"]
    pub fn load_checkpoint(
        &self,
        session_id: &str,
        turn_n: u32,
    ) -> Result<Option<Checkpoint>, IngotError> {
        checkpoint::load(&self.conn, session_id, turn_n)
    }

    /// Returns the checkpoint with the highest `turn_n` for `session_id`, or `None`
    /// if no checkpoints exist.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned checkpoint"]
    pub fn latest_checkpoint(&self, session_id: &str) -> Result<Option<Checkpoint>, IngotError> {
        checkpoint::latest(&self.conn, session_id)
    }

    /// Returns all ordinary (non-compaction) checkpoints for `session_id`,
    /// ordered by turn number ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned checkpoints"]
    pub fn list_checkpoints(&self, session_id: &str) -> Result<Vec<Checkpoint>, IngotError> {
        checkpoint::list(&self.conn, session_id)
    }

    /// Returns all compaction checkpoints for `session_id`, ordered by
    /// `created_at` ascending. Each carries a distinct `compaction_id`, so a
    /// session retains every compaction rather than overwriting the previous one.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned compaction checkpoints"]
    pub fn list_compaction_checkpoints(
        &self,
        session_id: &str,
    ) -> Result<Vec<Checkpoint>, IngotError> {
        checkpoint::list_compactions(&self.conn, session_id)
    }

    /// Atomically rolls back a session to `turn_n`, pruning all later checkpoints.
    ///
    /// Loads the checkpoint at `turn_n` and, within the same `SQLite` transaction,
    /// deletes every checkpoint for `session_id` with a turn number greater than
    /// `turn_n`. Returns `Ok(Some(checkpoint))` on success, or `Ok(None)` when no
    /// checkpoint exists at the requested turn (no rows are modified in that case).
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if any SQL operation fails.
    #[must_use = "check the Result to confirm the rollback succeeded"]
    pub fn rollback_session(
        &self,
        session_id: &str,
        turn_n: u32,
    ) -> Result<Option<Checkpoint>, IngotError> {
        checkpoint::rollback_session(&self.conn, session_id, turn_n)
    }

    // cost_ledger ------------------------------------------------------------

    /// Appends a [`CostEntry`] to the cost ledger.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails.
    #[must_use = "check the Result to confirm the cost entry was recorded"]
    pub fn insert_cost(&self, entry: &CostEntry) -> Result<(), IngotError> {
        cost::insert(&self.conn, entry)
    }

    /// Returns the exact total cost (microdollars) for all entries in
    /// `session_id`.
    ///
    /// Returns `Microdollars::from_micros(0)` when no entries exist.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned sum"]
    pub fn session_cost(&self, session_id: &str) -> Result<smedja_types::Microdollars, IngotError> {
        cost::session_total(&self.conn, session_id)
    }

    /// Returns per-model/runner aggregate rows for `session_id`, sorted by descending cost.
    ///
    /// Returns an empty vec when no entries exist.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned rows"]
    pub fn session_cost_entries(&self, session_id: &str) -> Result<Vec<CostRow>, IngotError> {
        cost::session_cost_entries(&self.conn, session_id)
    }

    /// Returns the model name from the most recent cost entry for `session_id`.
    ///
    /// Returns `None` when no cost entries exist.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result to determine the active model"]
    pub fn session_last_model(&self, session_id: &str) -> Result<Option<String>, IngotError> {
        cost::last_model(&self.conn, session_id)
    }

    /// Records a [`TokensSavedEntry`] on the tokens-saved ledger.
    ///
    /// Savings are kept separate from the billed `cost_ledger` so billed totals
    /// stay exact.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails.
    #[must_use = "check the Result to confirm the tokens-saved entry was recorded"]
    pub fn insert_tokens_saved(&self, entry: &TokensSavedEntry) -> Result<(), IngotError> {
        cost::insert_tokens_saved(&self.conn, entry)
    }

    /// Returns the total tokens saved by filtering for `session_id`.
    ///
    /// Returns `0` when no entries exist.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned total"]
    pub fn session_tokens_saved(&self, session_id: &str) -> Result<i64, IngotError> {
        cost::session_tokens_saved(&self.conn, session_id)
    }

    /// Returns the sum of `tokens_saved` grouped by `source` for `session_id`,
    /// ordered by `source`.
    ///
    /// Each tuple is `(source, summed_tokens_saved)`. Cache savings
    /// (`source = 'cache'`) stay distinct from compression savings.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned per-source sums"]
    pub fn session_tokens_saved_by_source(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, i64)>, IngotError> {
        cost::session_tokens_saved_by_source(&self.conn, session_id)
    }

    // metrics_rollups --------------------------------------------------------

    /// Computes time-tiered metrics buckets for `tier` over `[since, until)`.
    ///
    /// Aggregates tokens, cost, and turns from `cost_ledger` and error counts
    /// from `audit_events` (`status = 'error'`) per `(bucket, runner)`, merging
    /// the two on `(bucket_start, runner)`. Buckets are computed on read from the
    /// source rows — there is no staleness and no background writer. Results are
    /// ordered by `bucket_start` then `runner`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if either source query fails.
    #[must_use = "check the Result and inspect the returned buckets"]
    pub fn metrics_rollup(
        &self,
        tier: metrics_rollup::RollupTier,
        since: smedja_types::Timestamp,
        until: smedja_types::Timestamp,
    ) -> Result<Vec<MetricsBucket>, IngotError> {
        metrics_rollup::compute(&self.conn, tier, since, until)
    }

    /// Upserts the computed buckets for `tier` over `[epoch, until)` into the
    /// `metrics_rollups` cache, keyed on `(tier, bucket_start, runner)`.
    ///
    /// Materialises every bucket up to (but not including) `until`. Idempotent:
    /// re-running with the same `until` reproduces identical rows, and the stored
    /// rows equal `metrics_rollup(tier, epoch, until)`. The returned buckets are
    /// exactly what was stored.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the source queries or the upsert fail.
    #[must_use = "check the Result to confirm the rollups were materialised"]
    pub fn materialise_rollups(
        &self,
        tier: metrics_rollup::RollupTier,
        until: smedja_types::Timestamp,
    ) -> Result<Vec<MetricsBucket>, IngotError> {
        metrics_rollup::materialise(
            &self.conn,
            tier,
            smedja_types::Timestamp::from_micros(0),
            until,
        )
    }

    // savings_rollup ---------------------------------------------------------

    /// Computes time-tiered savings buckets for `tier` over `[since, until)`.
    ///
    /// Aggregates `tokens_saved` from `tokens_saved_ledger` per
    /// `(bucket, source)`, reusing [`RollupTier::bucket_start`] so savings
    /// buckets align with the billed buckets in [`Self::metrics_rollup`].
    /// Results are ordered by `bucket_start` then `source`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the source query fails.
    #[must_use = "check the Result and inspect the returned buckets"]
    pub fn savings_rollup(
        &self,
        tier: metrics_rollup::RollupTier,
        since: smedja_types::Timestamp,
        until: smedja_types::Timestamp,
    ) -> Result<Vec<SavingsBucket>, IngotError> {
        savings_rollup::compute(&self.conn, tier, since, until)
    }

    /// Computes the efficiency ratio `saved / (saved + billed_input)` over
    /// `[since, until)`.
    ///
    /// `saved` is the all-source `tokens_saved` sum; `billed_input` is the
    /// `cost_ledger.input_tok` sum over the same range. Returns `0.0` for an
    /// empty window. The `tier` argument is accepted for surface symmetry with
    /// [`Self::savings_rollup`]; the ratio is computed over the whole window.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if either source query fails.
    #[must_use = "check the Result and inspect the returned ratio"]
    pub fn efficiency_ratio(
        &self,
        tier: metrics_rollup::RollupTier,
        since: smedja_types::Timestamp,
        until: smedja_types::Timestamp,
    ) -> Result<f64, IngotError> {
        let _ = tier;
        savings_rollup::efficiency_ratio(&self.conn, since, until)
    }

    /// Computes the full [`SavingsSummary`] for `tier` over `[since, until)`.
    ///
    /// Carries the per-`(bucket, source)` rows plus the headline split:
    /// compression total (`filter` + `crusher` + `cold-context`) and cache total
    /// kept as separate figures, never summed, and the efficiency ratio.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if any source query fails.
    #[must_use = "check the Result and inspect the returned summary"]
    pub fn savings_summary(
        &self,
        tier: metrics_rollup::RollupTier,
        since: smedja_types::Timestamp,
        until: smedja_types::Timestamp,
    ) -> Result<SavingsSummary, IngotError> {
        savings_rollup::summary(&self.conn, tier, since, until)
    }

    // JSONL export / import --------------------------------------------------

    /// Exports tasks and their associated audit events as a JSONL stream.
    ///
    /// Each element in the returned [`Vec`] is a JSON object with a `"type"`
    /// field set to either `"task"` or `"audit_event"`, followed by all the
    /// fields of the respective struct.
    ///
    /// When `change` is `Some(name)`, only tasks whose `title` contains `name`
    /// (case-sensitive substring match) are exported together with their audit
    /// events.  When `change` is `None`, all tasks and all audit events are
    /// exported.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if any query fails, or [`IngotError::Json`]
    /// if serialisation fails.
    #[must_use = "check the Result and use the returned JSONL records"]
    pub fn export_jsonl(&self, change: Option<&str>) -> Result<Vec<serde_json::Value>, IngotError> {
        let tasks = self.list_tasks(None)?;
        let mut out: Vec<serde_json::Value> = Vec::new();

        for t in tasks {
            if let Some(name) = change {
                if !t.title.contains(name) {
                    continue;
                }
            }

            let mut task_obj = serde_json::to_value(&t)?;
            task_obj["type"] = serde_json::Value::String("task".to_owned());
            out.push(task_obj);

            if let Some(ref sid) = t.session_id {
                let events = self.list_audit_events(sid)?;
                for ev in events {
                    let mut ev_obj = serde_json::to_value(&ev)?;
                    ev_obj["type"] = serde_json::Value::String("audit_event".to_owned());
                    out.push(ev_obj);
                }
            }
        }

        Ok(out)
    }

    /// Imports tasks and audit events from a JSONL stream.
    ///
    /// Each element must be a JSON object with a `"type"` field of either
    /// `"task"` or `"audit_event"`.  Objects whose type field is unrecognised
    /// are silently skipped.  Rows that conflict with an existing primary key
    /// (`INSERT OR IGNORE`) are skipped so that re-running an import is
    /// idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Json`] if an object cannot be deserialised, or
    /// [`IngotError::Db`] if a database INSERT fails.
    #[must_use = "check the Result and inspect the returned import count"]
    pub fn import_jsonl(&self, records: &[serde_json::Value]) -> Result<usize, IngotError> {
        let mut imported = 0usize;
        for rec in records {
            match rec["type"].as_str() {
                Some("task") => {
                    let t: Task = serde_json::from_value(rec.clone())?;
                    let result = self.conn.execute(
                        "INSERT OR IGNORE INTO tasks \
                         (id, title, description, status, created_at, session_id, response) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        rusqlite::params![
                            t.id.to_string(),
                            t.title,
                            t.description,
                            t.status,
                            t.created_at.as_micros(),
                            t.session_id,
                            t.response,
                        ],
                    )?;
                    imported += result;
                }
                Some("audit_event") => {
                    let ev: AuditEvent = serde_json::from_value(rec.clone())?;
                    let result = self.conn.execute(
                        "INSERT OR IGNORE INTO audit_events \
                         (id, ts, session_id, turn_id, action_type, actor, tool_name, \
                          input_tok, output_tok, latency_ms, traceparent, tier, role_id, \
                          conversation_id, trace_id, span_id, parent_span_id, \
                          agent_name, operation_name, status, error_kind, error_count, \
                          tool_call_id) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, \
                                 ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
                        rusqlite::params![
                            ev.id.to_string(),
                            ev.ts.as_micros(),
                            ev.session_id,
                            ev.turn_id,
                            ev.action_type,
                            ev.actor,
                            ev.tool_name,
                            ev.input_tok,
                            ev.output_tok,
                            ev.latency_ms,
                            ev.traceparent,
                            ev.tier,
                            ev.role_id,
                            ev.conversation_id,
                            ev.trace_id,
                            ev.span_id,
                            ev.parent_span_id,
                            ev.agent_name,
                            ev.operation_name,
                            ev.status,
                            ev.error_kind,
                            ev.error_count,
                            ev.tool_call_id,
                        ],
                    )?;
                    imported += result;
                }
                _ => {
                    tracing::debug!(
                        record = ?rec,
                        "import_jsonl: skipping record with unrecognised type"
                    );
                }
            }
        }
        Ok(imported)
    }

    // token_snapshots --------------------------------------------------------

    /// Saves a [`TokenSnapshot`], replacing any existing snapshot for the same
    /// `(session_id, turn_n)` pair.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the upsert fails.
    #[must_use = "check the Result to confirm the snapshot was saved"]
    pub fn save_token_snapshot(&self, snap: &TokenSnapshot) -> Result<(), IngotError> {
        token_snapshot::save(&self.conn, snap)
    }

    /// Returns all [`TokenSnapshot`]s for `session_id`, ordered by `turn_n` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned snapshots"]
    pub fn session_token_snapshots(
        &self,
        session_id: &str,
    ) -> Result<Vec<TokenSnapshot>, IngotError> {
        token_snapshot::list_by_session(&self.conn, session_id)
    }

    // loops ------------------------------------------------------------------

    /// Inserts a new [`LoopRecord`] into the `loops` table.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails (e.g. duplicate `id`).
    #[must_use = "check the Result to confirm the loop record was created"]
    pub fn create_loop(&self, rec: &LoopRecord) -> Result<(), IngotError> {
        loop_state::insert(&self.conn, rec)
    }

    /// Returns the spec-first methodology state for `session_id`, or the
    /// all-false default when no row exists.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned methodology state"]
    pub fn get_methodology_state(&self, session_id: &str) -> Result<MethodologyState, IngotError> {
        methodology::get(&self.conn, session_id)
    }

    /// Sets the `spec_recorded` flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPSERT fails.
    #[must_use = "check the Result to confirm the flag was set"]
    pub fn set_spec_recorded(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        methodology::set_spec_recorded(&self.conn, session_id, value)
    }

    /// Sets the `approval_recorded` flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPSERT fails.
    #[must_use = "check the Result to confirm the flag was set"]
    pub fn set_approval_recorded(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        methodology::set_approval_recorded(&self.conn, session_id, value)
    }

    /// Sets the per-session `no_spec_gate` escape-hatch flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPSERT fails.
    #[must_use = "check the Result to confirm the flag was set"]
    pub fn set_no_spec_gate(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        methodology::set_no_spec_gate(&self.conn, session_id, value)
    }

    /// Retrieves a [`LoopRecord`] by `id`, returning `None` when not found.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned loop record"]
    pub fn get_loop(&self, id: &str) -> Result<Option<LoopRecord>, IngotError> {
        loop_state::get(&self.conn, id)
    }

    /// Updates the `status` and `updated_at` fields for the loop with `id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the loop status was updated"]
    pub fn update_loop_status(
        &self,
        id: &str,
        status: &str,
        updated_at: smedja_types::Timestamp,
    ) -> Result<(), IngotError> {
        loop_state::update_status(&self.conn, id, status, updated_at)
    }

    /// Returns all [`LoopRecord`]s for `change_name`, ordered by `created_at` descending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned loop records"]
    pub fn list_loops(&self, change_name: &str) -> Result<Vec<LoopRecord>, IngotError> {
        loop_state::list_by_change(&self.conn, change_name)
    }

    /// Updates `current_slice` and `updated_at` for the loop with `id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the slice was updated"]
    pub fn update_loop_slice(
        &self,
        id: &str,
        current_slice: i64,
        updated_at: smedja_types::Timestamp,
    ) -> Result<(), IngotError> {
        loop_state::update_slice(&self.conn, id, current_slice, updated_at)
    }

    /// Returns all [`LoopRecord`]s, optionally filtered by `status`.
    ///
    /// Pass `None` to return all loops. Pass `Some("retired")` (or any other valid
    /// status string) to restrict the result set. Results are ordered by
    /// `created_at` descending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned loop records"]
    pub fn list_loops_by_status(
        &self,
        status: Option<&str>,
    ) -> Result<Vec<LoopRecord>, IngotError> {
        loop_state::list_by_status(&self.conn, status)
    }

    // prompt_hashes ----------------------------------------------------------

    /// Records a prompt content hash for `(change, role)` at the current time.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails.
    #[must_use = "check the Result to confirm the hash was saved"]
    pub fn save_prompt_hash(&self, change: &str, role: &str, hash: &str) -> Result<(), IngotError> {
        prompt_hash::save(&self.conn, change, role, hash, now_epoch())
    }

    /// Returns the most recent prompt hash for `(change, role)`, or `None` when
    /// no record exists.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned hash"]
    pub fn get_prompt_hash(&self, change: &str, role: &str) -> Result<Option<String>, IngotError> {
        prompt_hash::get_latest(&self.conn, change, role)
    }

    /// Returns all prompt hash records for `change`, ordered by `ts` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned records"]
    pub fn list_prompt_hashes(&self, change: &str) -> Result<Vec<PromptHashRecord>, IngotError> {
        prompt_hash::list_by_change(&self.conn, change)
    }

    // audit_events (all) -----------------------------------------------------

    /// Returns all [`AuditEvent`]s ordered by `ts` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned events"]
    pub fn list_all_audit_events(&self) -> Result<Vec<AuditEvent>, IngotError> {
        audit::list_all(&self.conn)
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

/// Returns `true` for DDL errors that are safe to ignore for idempotency
/// (duplicate column, table already exists). Real failures (wrong syntax,
/// constraint violations, etc.) return `false` and must be propagated.
fn is_idempotent_ddl_error(e: &rusqlite::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("duplicate column name")
        || msg.contains("already exists")
        || msg.contains("table") && msg.contains("already")
}

/// Returns the current time as a Unix epoch `f64`.
fn now_epoch() -> f64 {
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
    fn migrate_is_idempotent_on_same_connection() {
        let ingot = Ingot::open_in_memory().unwrap();
        ingot.migrate().unwrap();
        ingot.migrate().unwrap();
    }

    #[test]
    fn schema_version_row_exists_after_open() {
        let ingot = Ingot::open_in_memory().unwrap();
        let version: i64 = ingot
            .conn
            .query_row(
                "SELECT version FROM schema_version WHERE version = ?1",
                rusqlite::params![SCHEMA_VERSION],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    // export / import round-trip ---------------------------------------------

    fn make_task(title: &str) -> Task {
        Task {
            id: uuid::Uuid::new_v4(),
            title: title.to_owned(),
            description: String::new(),
            status: "planned".to_owned(),
            created_at: smedja_types::Timestamp::from_secs_f64(1_700_000_000.0),
            session_id: None,
            response: None,
        }
    }

    fn make_audit_event(session_id: &str) -> AuditEvent {
        AuditEvent {
            id: uuid::Uuid::new_v4(),
            ts: smedja_types::Timestamp::from_secs_f64(1_700_000_001.0),
            session_id: session_id.to_owned(),
            action_type: "tool_exec".to_owned(),
            actor: "coder".to_owned(),
            tool_name: Some("bash".to_owned()),
            input_tok: 10,
            output_tok: 5,
            latency_ms: 42,
            ..AuditEvent::default()
        }
    }

    #[test]
    fn export_jsonl_returns_task_rows() {
        let ingot = Ingot::open_in_memory().unwrap();
        let t = make_task("fix the bug");
        ingot.create_task(&t).unwrap();

        let records = ingot.export_jsonl(None).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["type"], "task");
        assert_eq!(records[0]["title"], "fix the bug");
    }

    #[test]
    fn export_jsonl_filters_by_change_name() {
        let ingot = Ingot::open_in_memory().unwrap();
        ingot.create_task(&make_task("fix alpha")).unwrap();
        ingot.create_task(&make_task("fix beta")).unwrap();

        let records = ingot.export_jsonl(Some("alpha")).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["title"], "fix alpha");
    }

    #[test]
    fn export_import_round_trip_restores_same_rows() {
        let source = Ingot::open_in_memory().unwrap();
        let task = make_task("round trip task");
        source.create_task(&task).unwrap();

        let records = source.export_jsonl(None).unwrap();
        assert!(!records.is_empty());

        let dest = Ingot::open_in_memory().unwrap();
        let imported = dest.import_jsonl(&records).unwrap();
        assert_eq!(imported, 1);

        let tasks = dest.list_tasks(None).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, task.id);
        assert_eq!(tasks[0].title, "round trip task");
    }

    #[test]
    fn import_jsonl_is_idempotent_on_duplicate_ids() {
        let ingot = Ingot::open_in_memory().unwrap();
        let task = make_task("idempotent task");
        ingot.create_task(&task).unwrap();

        let records = ingot.export_jsonl(None).unwrap();

        let first = ingot.import_jsonl(&records).unwrap();
        assert_eq!(first, 0, "rows already exist; INSERT OR IGNORE should skip");
        let tasks = ingot.list_tasks(None).unwrap();
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn export_jsonl_includes_audit_events_for_session_tasks() {
        let ingot = Ingot::open_in_memory().unwrap();

        let session = crate::session::Session {
            id: uuid::Uuid::new_v4(),
            created_at: smedja_types::Timestamp::from_secs_f64(1_700_000_000.0),
            updated_at: smedja_types::Timestamp::from_secs_f64(1_700_000_000.0),
            status: "active".to_owned(),
            task_id: None,
            mode: None,
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        };
        ingot.create_session(&session).unwrap();

        let mut task = make_task("session task");
        task.session_id = Some(session.id.to_string());
        ingot.create_task(&task).unwrap();

        let ev = make_audit_event(&session.id.to_string());
        ingot.insert_audit_event(&ev).unwrap();

        let records = ingot.export_jsonl(None).unwrap();
        assert_eq!(records.len(), 2);
        let types: Vec<&str> = records.iter().filter_map(|r| r["type"].as_str()).collect();
        assert!(types.contains(&"task"));
        assert!(types.contains(&"audit_event"));
    }

    #[test]
    fn import_jsonl_skips_unknown_types() {
        let ingot = Ingot::open_in_memory().unwrap();
        let unknown = serde_json::json!({ "type": "unknown_record", "data": 42 });
        let imported = ingot.import_jsonl(&[unknown]).unwrap();
        assert_eq!(imported, 0);
    }

    // section 2 timeline tests -----------------------------------------------

    #[test]
    fn fresh_db_has_new_columns() {
        let ingot = Ingot::open_in_memory().unwrap();
        let ev = AuditEvent {
            id: uuid::Uuid::new_v4(),
            session_id: "sess-1".into(),
            conversation_id: Some("conv-001".into()),
            trace_id: Some("abc123".into()),
            span_id: Some("def456".into()),
            ..AuditEvent::default()
        };
        ingot.insert_audit_event(&ev).unwrap();
    }

    #[test]
    fn migrate_existing_db_adds_new_columns() {
        // Create a connection with the OLD schema (no conversation_id column).
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE audit_events (
                id TEXT PRIMARY KEY,
                ts REAL NOT NULL,
                session_id TEXT NOT NULL,
                turn_id TEXT,
                action_type TEXT NOT NULL DEFAULT '',
                actor TEXT NOT NULL DEFAULT '',
                tool_name TEXT,
                input_tok INTEGER NOT NULL DEFAULT 0,
                output_tok INTEGER NOT NULL DEFAULT 0,
                latency_ms INTEGER NOT NULL DEFAULT 0,
                traceparent TEXT,
                tier TEXT,
                role_id TEXT
            );
            CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
            INSERT INTO schema_version VALUES (1);",
        )
        .unwrap();
        // Re-run the new-column ALTER TABLE statements manually to simulate migration.
        for col in &[
            "conversation_id TEXT",
            "trace_id TEXT",
            "span_id TEXT",
            "parent_span_id TEXT",
            "agent_name TEXT",
            "operation_name TEXT",
            "status TEXT",
            "error_kind TEXT",
            "error_count INTEGER",
            "tool_call_id TEXT",
        ] {
            let _ = conn.execute(&format!("ALTER TABLE audit_events ADD COLUMN {col}"), []);
        }
        // Verify all new columns exist by inserting an event with the new fields.
        conn.execute(
            "INSERT INTO audit_events (id, ts, session_id, action_type, actor, conversation_id, trace_id, span_id, status)
             VALUES ('test-id', 1.0, 'sess-1', 'turn_start', 'user', 'conv-001', 'tid-1', 'sid-1', 'ok')",
            [],
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn migrate_backfills_real_seconds_to_integer_micros_idempotently() {
        // Seed an OLD-schema database: REAL seconds timestamps and REAL dollars,
        // table-level UNIQUE(session_id, turn_n) on checkpoints, and a
        // schema_migrations high-water mark below the new backfill versions.
        let sid = uuid::Uuid::new_v4().to_string();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at REAL NOT NULL);
             INSERT INTO schema_migrations (version, applied_at) VALUES (18, 0.0);
             CREATE TABLE sessions (
                 id TEXT PRIMARY KEY, created_at REAL NOT NULL, updated_at REAL NOT NULL,
                 status TEXT NOT NULL DEFAULT 'active', task_id TEXT, mode TEXT,
                 cowork_mode INTEGER NOT NULL DEFAULT 0, workspace_root TEXT, model_override TEXT,
                 runner_override TEXT, title TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE cost_ledger (
                 id TEXT PRIMARY KEY, session_id TEXT NOT NULL, turn_n INTEGER NOT NULL,
                 runner TEXT NOT NULL, model TEXT NOT NULL, input_tok INTEGER NOT NULL DEFAULT 0,
                 output_tok INTEGER NOT NULL DEFAULT 0, cost_usd REAL NOT NULL DEFAULT 0.0,
                 created_at REAL NOT NULL
             );
             CREATE TABLE checkpoints (
                 id TEXT PRIMARY KEY, session_id TEXT NOT NULL, turn_n INTEGER NOT NULL,
                 messages_json TEXT NOT NULL, created_at REAL NOT NULL,
                 UNIQUE(session_id, turn_n)
             );
             CREATE TABLE audit_events (
                 id TEXT PRIMARY KEY, ts REAL NOT NULL, session_id TEXT NOT NULL,
                 turn_id TEXT, action_type TEXT NOT NULL, actor TEXT NOT NULL,
                 tool_name TEXT, input_tok INTEGER NOT NULL DEFAULT 0,
                 output_tok INTEGER NOT NULL DEFAULT 0, latency_ms INTEGER NOT NULL DEFAULT 0,
                 traceparent TEXT, tier TEXT,
                 role_id TEXT, conversation_id TEXT, trace_id TEXT, span_id TEXT,
                 parent_span_id TEXT, agent_name TEXT, operation_name TEXT,
                 status TEXT, error_kind TEXT, error_count INTEGER, tool_call_id TEXT
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, created_at, updated_at, status) \
             VALUES (?1, 1700000000.5, 1700000000.5, 'active')",
            rusqlite::params![sid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cost_ledger (id, session_id, turn_n, runner, model, cost_usd, created_at) \
             VALUES ('c-old', ?1, 0, 'claude', 'claude-sonnet-4-6', 0.042, 1700000000.0)",
            rusqlite::params![sid],
        )
        .unwrap();
        let ck_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO checkpoints (id, session_id, turn_n, messages_json, created_at) \
             VALUES (?1, ?2, -1, '[]', 1700000000.0)",
            rusqlite::params![ck_id, sid],
        )
        .unwrap();

        let ingot = Ingot { conn };
        ingot.migrate().unwrap();

        // Timestamps converted from REAL seconds to INTEGER micros.
        let session = ingot.get_session(&sid).unwrap().unwrap();
        assert_eq!(
            session.created_at,
            smedja_types::Timestamp::from_micros(1_700_000_000_500_000)
        );

        // Money converted from REAL dollars to INTEGER microdollars.
        let total = ingot.session_cost(&sid).unwrap();
        assert_eq!(total, smedja_types::Microdollars::from_micros(42_000));

        // Legacy turn_n = -1 compaction row carries a generated compaction_id and
        // is retrievable as a compaction checkpoint.
        let compactions = ingot.list_compaction_checkpoints(&sid).unwrap();
        assert_eq!(compactions.len(), 1);
        assert!(compactions[0].compaction_id.is_some());

        // Re-running migrate() must NOT double-convert (version-gated idempotency).
        ingot.migrate().unwrap();
        let session_again = ingot.get_session(&sid).unwrap().unwrap();
        assert_eq!(
            session_again.created_at,
            smedja_types::Timestamp::from_micros(1_700_000_000_500_000)
        );
        let total_again = ingot.session_cost(&sid).unwrap();
        assert_eq!(total_again, smedja_types::Microdollars::from_micros(42_000));
        assert_eq!(ingot.list_compaction_checkpoints(&sid).unwrap().len(), 1);
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

    #[test]
    fn record_timeline_event_updates_rollup() {
        let ingot = Ingot::open_in_memory().unwrap();
        let ev = AuditEvent {
            id: uuid::Uuid::new_v4(),
            session_id: "sess-1".into(),
            conversation_id: Some("conv-test".into()),
            action_type: "llm".into(),
            ..AuditEvent::default()
        };
        ingot.record_timeline_event(&ev).unwrap();
        let rollups = ingot.recent_conversations(10).unwrap();
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].conversation_id, "conv-test");
        assert_eq!(rollups[0].llm_call_count, 1);
    }

    #[test]
    fn record_timeline_event_accumulates_across_calls() {
        let ingot = Ingot::open_in_memory().unwrap();
        let conv_id = Some("conv-accum".to_owned());

        let llm_ev = AuditEvent {
            id: uuid::Uuid::new_v4(),
            session_id: "s".into(),
            conversation_id: conv_id.clone(),
            action_type: "llm".into(),
            input_tok: 100,
            output_tok: 50,
            ..AuditEvent::default()
        };
        let tool_ev = AuditEvent {
            id: uuid::Uuid::new_v4(),
            session_id: "s".into(),
            conversation_id: conv_id.clone(),
            action_type: "tool".into(),
            input_tok: 10,
            output_tok: 5,
            ..AuditEvent::default()
        };
        let err_ev = AuditEvent {
            id: uuid::Uuid::new_v4(),
            session_id: "s".into(),
            conversation_id: conv_id,
            action_type: "llm".into(),
            status: Some("error".into()),
            input_tok: 20,
            output_tok: 0,
            ..AuditEvent::default()
        };

        ingot.record_timeline_event(&llm_ev).unwrap();
        ingot.record_timeline_event(&tool_ev).unwrap();
        ingot.record_timeline_event(&err_ev).unwrap();

        let rollups = ingot.recent_conversations(10).unwrap();
        assert_eq!(rollups.len(), 1);
        let r = &rollups[0];
        assert_eq!(r.llm_call_count, 2);
        assert_eq!(r.tool_call_count, 1);
        assert_eq!(r.failure_count, 1);
        assert_eq!(r.input_token_total, 130);
        assert_eq!(r.output_token_total, 55);
    }

    #[test]
    fn record_timeline_event_without_conversation_id_skips_rollup() {
        let ingot = Ingot::open_in_memory().unwrap();
        let ev = AuditEvent {
            id: uuid::Uuid::new_v4(),
            session_id: "sess-no-conv".into(),
            action_type: "llm".into(),
            ..AuditEvent::default()
        };
        ingot.record_timeline_event(&ev).unwrap();
        let rollups = ingot.recent_conversations(10).unwrap();
        assert!(rollups.is_empty());
    }

    #[test]
    fn conversation_timeline_returns_ordered_events() {
        let ingot = Ingot::open_in_memory().unwrap();
        for i in 0..3 {
            let ev = AuditEvent {
                id: uuid::Uuid::new_v4(),
                session_id: format!("sess-{i}"),
                conversation_id: Some("conv-order".into()),
                ..AuditEvent::default()
            };
            ingot.record_timeline_event(&ev).unwrap();
        }
        let events = ingot.conversation_timeline("conv-order").unwrap();
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn failed_events_returns_only_error_status() {
        let ingot = Ingot::open_in_memory().unwrap();
        let conv_id = Some("conv-fail".to_owned());

        let ok_ev = AuditEvent {
            id: uuid::Uuid::new_v4(),
            session_id: "s".into(),
            conversation_id: conv_id.clone(),
            status: Some("ok".into()),
            ..AuditEvent::default()
        };
        let err_ev = AuditEvent {
            id: uuid::Uuid::new_v4(),
            session_id: "s".into(),
            conversation_id: conv_id,
            status: Some("error".into()),
            ..AuditEvent::default()
        };

        ingot.record_timeline_event(&ok_ev).unwrap();
        ingot.record_timeline_event(&err_ev).unwrap();

        let failures = ingot.failed_events("conv-fail").unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].status.as_deref(), Some("error"));
    }

    #[test]
    fn recent_conversations_respects_limit() {
        let ingot = Ingot::open_in_memory().unwrap();
        for i in 0..5 {
            let ev = AuditEvent {
                id: uuid::Uuid::new_v4(),
                session_id: "s".into(),
                conversation_id: Some(format!("conv-{i}")),
                ..AuditEvent::default()
            };
            ingot.record_timeline_event(&ev).unwrap();
        }
        let rollups = ingot.recent_conversations(3).unwrap();
        assert_eq!(rollups.len(), 3);
    }

    // 9.4 — Ingot timeline tests -----------------------------------------------

    #[test]
    fn conversation_timeline_returns_events_in_order() {
        let ingot = Ingot::open_in_memory().unwrap();
        let make_ev = |action: &str, ts: f64| AuditEvent {
            id: uuid::Uuid::new_v4(),
            ts: smedja_types::Timestamp::from_secs_f64(ts),
            session_id: "s".into(),
            action_type: action.to_string(),
            actor: "smdjad".into(),
            conversation_id: Some("conv-order-2".into()),
            ..AuditEvent::default()
        };
        ingot
            .record_timeline_event(&make_ev("turn_start", 1.0))
            .unwrap();
        ingot
            .record_timeline_event(&make_ev("tool_exec", 2.0))
            .unwrap();
        ingot
            .record_timeline_event(&make_ev("turn_end", 3.0))
            .unwrap();
        let events = ingot.conversation_timeline("conv-order-2").unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].action_type, "turn_start");
        assert_eq!(events[2].action_type, "turn_end");
    }

    #[test]
    fn failed_events_filters_by_status() {
        let ingot = Ingot::open_in_memory().unwrap();
        let ok_ev = AuditEvent {
            id: uuid::Uuid::new_v4(),
            ts: smedja_types::Timestamp::from_secs_f64(1.0),
            session_id: "s".into(),
            action_type: "tool_exec".into(),
            actor: "smdjad".into(),
            conversation_id: Some("conv-fail-filter".into()),
            status: Some("ok".into()),
            ..AuditEvent::default()
        };
        let err_ev = AuditEvent {
            id: uuid::Uuid::new_v4(),
            ts: smedja_types::Timestamp::from_secs_f64(2.0),
            status: Some("error".into()),
            action_type: "tool_exec".into(),
            actor: "smdjad".into(),
            session_id: "s".into(),
            conversation_id: Some("conv-fail-filter".into()),
            ..AuditEvent::default()
        };
        ingot.record_timeline_event(&ok_ev).unwrap();
        ingot.record_timeline_event(&err_ev).unwrap();
        let fails = ingot.failed_events("conv-fail-filter").unwrap();
        assert_eq!(fails.len(), 1);
        assert_eq!(fails[0].status.as_deref(), Some("error"));
    }

    // --- prune_old_sessions + vacuum -----------------------------------------

    fn make_session(status: &str, updated_at: smedja_types::Timestamp) -> crate::session::Session {
        crate::session::Session {
            id: uuid::Uuid::new_v4(),
            created_at: updated_at,
            updated_at,
            status: status.to_owned(),
            title: String::new(),
            cowork_mode: false,
            task_id: None,
            mode: None,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        }
    }

    #[test]
    fn prune_old_sessions_removes_stale_terminal_sessions_and_cascades() {
        let ingot = Ingot::open_in_memory().unwrap();
        // Old complete session (timestamp year 2001) — must be pruned.
        let old_ts = smedja_types::Timestamp::from_secs_f64(1_000_000_000.0);
        let old_sess = make_session("complete", old_ts);
        ingot.create_session(&old_sess).unwrap();

        // Recent active session — must survive (wrong status).
        let new_sess = make_session("active", smedja_types::Timestamp::now());
        ingot.create_session(&new_sess).unwrap();

        // Dependent rows for old session.
        let mut old_task = make_task("old-task");
        old_task.session_id = Some(old_sess.id.to_string());
        ingot.create_task(&old_task).unwrap();
        let mut old_ev = make_audit_event(&old_sess.id.to_string());
        old_ev.id = uuid::Uuid::new_v4();
        ingot.insert_audit_event(&old_ev).unwrap();

        // Dependent rows for new session.
        let mut new_task = make_task("new-task");
        new_task.session_id = Some(new_sess.id.to_string());
        ingot.create_task(&new_task).unwrap();

        // Prune sessions older than 0 days (cutoff = now → evicts anything in the past).
        let deleted = ingot.prune_old_sessions(0).unwrap();
        assert_eq!(
            deleted, 1,
            "exactly the old complete session must be pruned"
        );

        // Old session and its dependents must be gone.
        assert!(ingot
            .get_session(&old_sess.id.to_string())
            .unwrap()
            .is_none());
        let tasks = ingot.list_tasks(None).unwrap();
        assert!(
            tasks.iter().all(|t| t.id != old_task.id),
            "task belonging to pruned session must be cascaded"
        );
        // New session and its task survive.
        assert!(ingot
            .get_session(&new_sess.id.to_string())
            .unwrap()
            .is_some());
        assert!(tasks.iter().any(|t| t.id == new_task.id));
    }

    #[test]
    fn prune_old_sessions_preserves_recent_complete_sessions() {
        let ingot = Ingot::open_in_memory().unwrap();
        let sess = make_session("complete", smedja_types::Timestamp::now());
        ingot.create_session(&sess).unwrap();
        // Prune sessions older than 30 days — brand-new session must survive.
        let deleted = ingot.prune_old_sessions(30).unwrap();
        assert_eq!(deleted, 0);
        assert!(ingot.get_session(&sess.id.to_string()).unwrap().is_some());
    }

    #[test]
    fn prune_old_sessions_does_not_prune_active_sessions_regardless_of_age() {
        let ingot = Ingot::open_in_memory().unwrap();
        let old_ts = smedja_types::Timestamp::from_secs_f64(1_000_000_000.0);
        let sess = make_session("active", old_ts);
        ingot.create_session(&sess).unwrap();
        let deleted = ingot.prune_old_sessions(0).unwrap();
        assert_eq!(deleted, 0, "active sessions must never be pruned");
    }

    #[test]
    fn vacuum_completes_without_error() {
        let ingot = Ingot::open_in_memory().unwrap();
        ingot.vacuum().unwrap();
    }
}
