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
pub mod loop_state;
pub mod mcp;
pub mod session;
pub mod task;

pub use audit::AuditEvent;
pub use checkpoint::Checkpoint;
pub use cost::CostEntry;
pub use error::IngotError;
pub use loop_state::LoopRecord;
pub use mcp::McpServer;
pub use session::Session;
pub use task::Task;

/// The current schema version applied by [`Ingot::migrate`].
const SCHEMA_VERSION: i64 = 1;

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
    fn migrate(&self) -> Result<(), IngotError> {
        self.conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY
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
                workspace_root TEXT
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
            ",
        )?;

        // Add response column to existing databases — SQLite returns an error if the
        // column already exists; we suppress it so migrate() stays idempotent.
        let _ = self
            .conn
            .execute_batch("ALTER TABLE tasks ADD COLUMN response TEXT;");

        // Add cowork_mode column to existing sessions tables — suppressed if already present.
        let _ = self.conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN cowork_mode INTEGER NOT NULL DEFAULT 0;",
        );

        // Add workspace_root column to existing sessions tables — suppressed if already present.
        let _ = self
            .conn
            .execute_batch("ALTER TABLE sessions ADD COLUMN workspace_root TEXT;");

        // Record (or ignore) the schema version marker.
        self.conn.execute(
            "INSERT OR IGNORE INTO schema_version (version) VALUES (?1)",
            rusqlite::params![SCHEMA_VERSION],
        )?;

        Ok(())
    }

    // ── audit_events ────────────────────────────────────────────────────────

    /// Appends an [`AuditEvent`] to the immutable audit log.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails.
    #[must_use = "check the Result to confirm the event was persisted"]
    pub fn insert_audit_event(&mut self, event: &AuditEvent) -> Result<(), IngotError> {
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

    // ── sessions ─────────────────────────────────────────────────────────────

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

    // ── mcp_servers ──────────────────────────────────────────────────────────

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

    // ── tasks ─────────────────────────────────────────────────────────────────

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

    // ── checkpoints ──────────────────────────────────────────────────────────

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

    // ── cost_ledger ──────────────────────────────────────────────────────────

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

    // ── loops ─────────────────────────────────────────────────────────────────

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
        // Calling open_in_memory twice must not fail.
        let _a = Ingot::open_in_memory().unwrap();
        let _b = Ingot::open_in_memory().unwrap();
    }

    #[test]
    fn migrate_is_idempotent_on_same_connection() {
        // Directly call migrate() a second time on the same connection — must not fail.
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
}
