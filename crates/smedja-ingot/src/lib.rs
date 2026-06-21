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
pub mod session;
pub mod task;

pub use audit::AuditEvent;
pub use checkpoint::Checkpoint;
pub use cost::CostEntry;
pub use error::IngotError;
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
                id          TEXT PRIMARY KEY,
                created_at  REAL NOT NULL,
                updated_at  REAL NOT NULL,
                status      TEXT NOT NULL DEFAULT 'active',
                task_id     TEXT,
                mode        TEXT
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
            ",
        )?;

        // Add response column to existing databases — SQLite returns an error if the
        // column already exists; we suppress it so migrate() stays idempotent.
        let _ = self
            .conn
            .execute_batch("ALTER TABLE tasks ADD COLUMN response TEXT;");

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
