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
pub mod loop_state;
pub mod mcp;
pub mod openspec_store;
pub mod prompt_hash;
pub mod session;
pub mod task;
pub mod token_snapshot;

pub use audit::AuditEvent;
pub use checkpoint::Checkpoint;
pub use cost::{CostEntry, CostRow};
pub use error::IngotError;
pub use guard::{classify as classify_command, is_safe as command_is_safe, CommandRisk};
pub use loop_state::LoopRecord;
pub use mcp::McpServer;
pub use openspec_store::OpenSpecStore;
pub use prompt_hash::PromptHashRecord;
pub use session::Session;
pub use task::Task;
pub use token_snapshot::TokenSnapshot;

/// The current schema version applied by [`Ingot::migrate`].
const SCHEMA_VERSION: i64 = 3;

/// Numbered migrations applied in sequence after the base DDL.
///
/// Each entry is `(version, sql)`. The `sql` may be a single statement or a
/// semicolon-separated batch. Migrations are applied in ascending version order
/// and recorded in `schema_migrations` so they are never applied twice.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, "ALTER TABLE tasks ADD COLUMN response TEXT;"),
    (2, "ALTER TABLE sessions ADD COLUMN cowork_mode INTEGER NOT NULL DEFAULT 0;"),
    (3, "ALTER TABLE sessions ADD COLUMN workspace_root TEXT;"),
    (4, "ALTER TABLE sessions ADD COLUMN model_override TEXT;"),
    (5, "ALTER TABLE sessions ADD COLUMN runner_override TEXT;"),
    (6, "ALTER TABLE audit_events ADD COLUMN role_id TEXT;"),
    (7, "ALTER TABLE audit_events ADD COLUMN conversation_id TEXT;"),
    (8, "ALTER TABLE audit_events ADD COLUMN trace_id TEXT;"),
    (9, "ALTER TABLE audit_events ADD COLUMN span_id TEXT;"),
    (10, "ALTER TABLE audit_events ADD COLUMN parent_span_id TEXT;"),
    (11, "ALTER TABLE audit_events ADD COLUMN agent_name TEXT;"),
    (12, "ALTER TABLE audit_events ADD COLUMN operation_name TEXT;"),
    (13, "ALTER TABLE audit_events ADD COLUMN status TEXT;"),
    (14, "ALTER TABLE audit_events ADD COLUMN error_kind TEXT;"),
    (15, "ALTER TABLE audit_events ADD COLUMN error_count INTEGER;"),
    (16, "ALTER TABLE audit_events ADD COLUMN tool_call_id TEXT;"),
    (17, "ALTER TABLE sessions ADD COLUMN title TEXT NOT NULL DEFAULT '';"),
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
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY
            );

            CREATE TABLE IF NOT EXISTS schema_migrations (
                version    INTEGER PRIMARY KEY,
                applied_at REAL NOT NULL
            );

            CREATE TABLE IF NOT EXISTS audit_events (
                id          TEXT PRIMARY KEY,
                ts          REAL NOT NULL,
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
                created_at     REAL NOT NULL,
                updated_at     REAL NOT NULL,
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
                created_at  REAL NOT NULL,
                session_id  TEXT,
                response    TEXT
            );

            CREATE TABLE IF NOT EXISTS checkpoints (
                id            TEXT PRIMARY KEY,
                session_id    TEXT NOT NULL,
                turn_n        INTEGER NOT NULL,
                messages_json TEXT NOT NULL,
                created_at    REAL NOT NULL,
                UNIQUE(session_id, turn_n)
            );

            CREATE TABLE IF NOT EXISTS cost_ledger (
                id          TEXT PRIMARY KEY,
                session_id  TEXT NOT NULL,
                turn_n      INTEGER NOT NULL,
                runner      TEXT NOT NULL,
                model       TEXT NOT NULL,
                input_tok   INTEGER NOT NULL DEFAULT 0,
                output_tok  INTEGER NOT NULL DEFAULT 0,
                cost_usd    REAL NOT NULL DEFAULT 0.0,
                created_at  REAL NOT NULL
            );

            CREATE TABLE IF NOT EXISTS loops (
                id            TEXT PRIMARY KEY,
                change_name   TEXT NOT NULL,
                status        TEXT NOT NULL DEFAULT 'planning',
                current_slice INTEGER NOT NULL DEFAULT 0,
                attempt       INTEGER NOT NULL DEFAULT 1,
                created_at    REAL NOT NULL,
                updated_at    REAL NOT NULL
            );

            CREATE TABLE IF NOT EXISTS turn_token_snapshots (
                id               TEXT PRIMARY KEY,
                session_id       TEXT NOT NULL,
                turn_n           INTEGER NOT NULL,
                input_tok        INTEGER NOT NULL DEFAULT 0,
                output_tok       INTEGER NOT NULL DEFAULT 0,
                cumulative_input INTEGER NOT NULL DEFAULT 0,
                cumulative_output INTEGER NOT NULL DEFAULT 0,
                created_at       REAL NOT NULL,
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
            // already present in the base CREATE TABLE DDL above.  We suppress the
            // error so migrate() stays idempotent across both fresh and pre-existing
            // databases.
            let _ = self.conn.execute_batch(sql);
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
    pub fn create_session(&mut self, session: &Session) -> Result<(), IngotError> {
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

    /// Deletes the session with the given `id`.
    ///
    /// Returns `true` if a row was deleted, `false` if no session with that `id`
    /// existed.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the DELETE fails.
    #[must_use = "check the Result to confirm the session was deleted"]
    pub fn delete_session(&mut self, id: &str) -> Result<bool, IngotError> {
        session::delete(&self.conn, id)
    }

    /// Updates the `status` of a session to `status` and records a new `updated_at`
    /// timestamp using the current Unix epoch.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the status was updated"]
    pub fn update_session_status(&mut self, id: &str, status: &str) -> Result<(), IngotError> {
        let now = now_epoch();
        session::update_status(&self.conn, id, status, now)
    }

    /// Sets the `cowork_mode` flag for the session identified by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the cowork mode was updated"]
    pub fn set_cowork_mode(&mut self, session_id: &str, enabled: bool) -> Result<(), IngotError> {
        self.conn.execute(
            "UPDATE sessions SET cowork_mode = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![i64::from(enabled), now_epoch(), session_id],
        )?;
        Ok(())
    }

    /// Sets the `workspace_root` filesystem path for the session identified by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the workspace root was updated"]
    pub fn update_session_workspace_root(
        &mut self,
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
    pub fn update_session_mode(&mut self, session_id: &str, mode: &str) -> Result<(), IngotError> {
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
        &mut self,
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
        &mut self,
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
        &mut self,
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
        &mut self,
        session_id: &str,
        enabled: bool,
    ) -> Result<(), IngotError> {
        session::update_cowork_mode(&self.conn, session_id, enabled)
    }

    // mcp_servers ------------------------------------------------------------

    /// Registers (or replaces) an [`McpServer`] in the registry.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT OR REPLACE fails.
    #[must_use = "check the Result to confirm the MCP server was registered"]
    pub fn register_mcp_server(&mut self, server: &McpServer) -> Result<(), IngotError> {
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
    pub fn remove_mcp_server(&mut self, id: &str) -> Result<(), IngotError> {
        mcp::remove(&self.conn, id)
    }

    /// Updates the cached tool list and refresh timestamp for the server identified
    /// by `name`. Sets `last_refresh` to the current Unix epoch.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the tool list was updated"]
    pub fn update_mcp_tools(&mut self, name: &str, tools_json: &str) -> Result<(), IngotError> {
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
    pub fn create_task(&mut self, task: &Task) -> Result<(), IngotError> {
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

    /// Updates the `status` field for the task identified by `id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the status was updated"]
    pub fn update_task_status(&mut self, id: &str, status: &str) -> Result<(), IngotError> {
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
    pub fn set_task_response(&mut self, id: &str, response: &str) -> Result<(), IngotError> {
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
    pub fn save_checkpoint(&mut self, cp: &Checkpoint) -> Result<(), IngotError> {
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

    /// Returns all checkpoints for `session_id`, ordered by turn number ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned checkpoints"]
    pub fn list_checkpoints(&self, session_id: &str) -> Result<Vec<Checkpoint>, IngotError> {
        checkpoint::list(&self.conn, session_id)
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
        &mut self,
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
    pub fn insert_cost(&mut self, entry: &CostEntry) -> Result<(), IngotError> {
        cost::insert(&self.conn, entry)
    }

    /// Returns the total `cost_usd` for all entries in `session_id`.
    ///
    /// Returns `0.0` when no entries exist.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned sum"]
    pub fn session_cost(&self, session_id: &str) -> Result<f64, IngotError> {
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
    pub fn import_jsonl(&mut self, records: &[serde_json::Value]) -> Result<usize, IngotError> {
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
                            t.created_at,
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
                            ev.ts,
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
    pub fn save_token_snapshot(&mut self, snap: &TokenSnapshot) -> Result<(), IngotError> {
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
    pub fn create_loop(&mut self, rec: &LoopRecord) -> Result<(), IngotError> {
        loop_state::insert(&self.conn, rec)
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
        &mut self,
        id: &str,
        status: &str,
        updated_at: f64,
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
        &mut self,
        id: &str,
        current_slice: i64,
        updated_at: f64,
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
    pub fn save_prompt_hash(
        &mut self,
        change: &str,
        role: &str,
        hash: &str,
    ) -> Result<(), IngotError> {
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
            created_at: 1_700_000_000.0,
            session_id: None,
            response: None,
        }
    }

    fn make_audit_event(session_id: &str) -> AuditEvent {
        AuditEvent {
            id: uuid::Uuid::new_v4(),
            ts: 1_700_000_001.0,
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
        let mut ingot = Ingot::open_in_memory().unwrap();
        let t = make_task("fix the bug");
        ingot.create_task(&t).unwrap();

        let records = ingot.export_jsonl(None).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["type"], "task");
        assert_eq!(records[0]["title"], "fix the bug");
    }

    #[test]
    fn export_jsonl_filters_by_change_name() {
        let mut ingot = Ingot::open_in_memory().unwrap();
        ingot.create_task(&make_task("fix alpha")).unwrap();
        ingot.create_task(&make_task("fix beta")).unwrap();

        let records = ingot.export_jsonl(Some("alpha")).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["title"], "fix alpha");
    }

    #[test]
    fn export_import_round_trip_restores_same_rows() {
        let mut source = Ingot::open_in_memory().unwrap();
        let task = make_task("round trip task");
        source.create_task(&task).unwrap();

        let records = source.export_jsonl(None).unwrap();
        assert!(!records.is_empty());

        let mut dest = Ingot::open_in_memory().unwrap();
        let imported = dest.import_jsonl(&records).unwrap();
        assert_eq!(imported, 1);

        let tasks = dest.list_tasks(None).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, task.id);
        assert_eq!(tasks[0].title, "round trip task");
    }

    #[test]
    fn import_jsonl_is_idempotent_on_duplicate_ids() {
        let mut ingot = Ingot::open_in_memory().unwrap();
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
        let mut ingot = Ingot::open_in_memory().unwrap();

        let session = crate::session::Session {
            id: uuid::Uuid::new_v4(),
            created_at: 1_700_000_000.0,
            updated_at: 1_700_000_000.0,
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
        let mut ingot = Ingot::open_in_memory().unwrap();
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
            ts,
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
            ts: 1.0,
            session_id: "s".into(),
            action_type: "tool_exec".into(),
            actor: "smdjad".into(),
            conversation_id: Some("conv-fail-filter".into()),
            status: Some("ok".into()),
            ..AuditEvent::default()
        };
        let err_ev = AuditEvent {
            id: uuid::Uuid::new_v4(),
            ts: 2.0,
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
}
