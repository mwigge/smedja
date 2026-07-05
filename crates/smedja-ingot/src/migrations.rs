//! Schema bootstrap and version-gated migrations.
//!
//! Owns the base DDL applied on every [`Ingot`] open, the numbered
//! [`MIGRATIONS`] table, and the machinery that applies them idempotently.

use crate::error::IngotError;
use crate::Ingot;

/// The current schema version recorded in the legacy `schema_version` marker
/// table. Derived from the number of numbered [`MIGRATIONS`] so it can never
/// drift from the migrations actually defined.
#[allow(clippy::cast_possible_wrap)] // MIGRATIONS.len() is a small compile-time constant
pub(crate) const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Numbered migrations applied in sequence after the base DDL.
///
/// Each entry is `(version, sql)`. The `sql` may be a single statement or a
/// semicolon-separated batch. Migrations are applied in ascending version order
/// and recorded in `schema_migrations` so they are never applied twice.
pub(crate) const MIGRATIONS: &[(i64, &str)] = &[
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

impl Ingot {
    /// Applies all `CREATE TABLE IF NOT EXISTS` statements, making schema bootstrap
    /// fully idempotent.
    #[allow(clippy::too_many_lines)] // DDL — length is inherent, not complexity
    pub(crate) fn migrate(&self) -> Result<(), IngotError> {
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
            // Apply the whole migration batch inside a savepoint. Only after
            // every statement has been applied (or has failed with an ignorable
            // idempotent error) do we record the version, so a migration whose
            // later statements were skipped is never marked complete.
            apply_migration_batch(&self.conn, version, sql)?;
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
}

/// Returns `true` for DDL errors that are safe to ignore for idempotency
/// (duplicate column, table already exists). Real failures (wrong syntax,
/// constraint violations, etc.) return `false` and must be propagated.
/// Applies a single migration's SQL batch inside a savepoint, executing it one
/// statement at a time.
///
/// Running statement-by-statement means an idempotent error (duplicate column,
/// table already exists) in one statement is ignored **without** skipping the
/// statements that follow it — a whole-batch `execute_batch` would abort at the
/// first error and leave later statements unapplied. Any genuine error rolls the
/// savepoint back and is returned, so the caller never records a half-applied
/// migration as complete. The savepoint is released only when the entire batch
/// has been applied.
pub(crate) fn apply_migration_batch(
    conn: &rusqlite::Connection,
    version: i64,
    sql: &str,
) -> Result<(), IngotError> {
    let sp_name = format!("migration_{version}");
    conn.execute_batch(&format!("SAVEPOINT \"{sp_name}\""))?;
    for stmt in sql.split(';') {
        if stmt.trim().is_empty() {
            continue;
        }
        match conn.execute_batch(stmt) {
            Ok(()) => {}
            // Ignorable: the statement's effect already exists. Nothing was
            // modified (these are prepare-time errors) so the savepoint stays
            // valid and the remaining statements still run.
            Err(ref e) if is_idempotent_ddl_error(e) => {}
            Err(e) => {
                conn.execute_batch(&format!("ROLLBACK TO \"{sp_name}\""))?;
                conn.execute_batch(&format!("RELEASE \"{sp_name}\""))?;
                return Err(IngotError::Db(e));
            }
        }
    }
    conn.execute_batch(&format!("RELEASE \"{sp_name}\""))?;
    Ok(())
}

fn is_idempotent_ddl_error(e: &rusqlite::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("duplicate column name")
        || msg.contains("already exists")
        || msg.contains("table") && msg.contains("already")
}

/// Returns the current time as a Unix epoch `f64`.
pub(crate) fn now_epoch() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
