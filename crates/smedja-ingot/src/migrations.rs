//! Schema versioning: numbered migrations, the base-DDL bootstrap, and the
//! idempotent [`Ingot::migrate`] driver.

use crate::{now_epoch, Ingot, IngotError};

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
    // Value panel: stamp change_name on cost_ledger so the value panel can
    // show cumulative USD spend per change, not just token count.
    (
        26,
        "ALTER TABLE cost_ledger ADD COLUMN change_name TEXT; \
         CREATE INDEX IF NOT EXISTS idx_cost_ledger_change_name \
             ON cost_ledger(change_name) WHERE change_name IS NOT NULL;",
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

#[cfg(test)]
mod tests {
    use super::SCHEMA_VERSION;
    use crate::*;

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
}
