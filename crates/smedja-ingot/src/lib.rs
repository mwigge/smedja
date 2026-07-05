//! `smedja-ingot` — `SQLite` persistence layer for the smedja multi-agent orchestration platform.
//!
//! Provides schema bootstrap, CRUD operations for audit events, sessions, tasks,
//! checkpoints, and cost ledger entries. All operations are synchronous; callers
//! running inside an async runtime should use [`tokio::task::spawn_blocking`] to
//! avoid blocking the executor thread.

pub mod audit;
pub mod checkpoint;
mod conversation;
pub mod cost;
pub mod error;
pub mod guard;
pub mod handle;
mod jsonl;
pub mod loop_state;
mod maintenance;
pub mod mcp;
pub mod methodology;
pub mod metrics_rollup;
mod migrations;
pub mod openspec_store;
pub mod prompt_hash;
pub mod savings_rollup;
pub mod session;
pub mod task;
pub mod token_snapshot;

pub use audit::AuditEvent;
pub use checkpoint::Checkpoint;
pub use conversation::{parse_traceparent, ConversationRollup};
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::{apply_migration_batch, SCHEMA_VERSION};

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
    fn single_event_conversation_has_agent_count_one() {
        let ingot = Ingot::open_in_memory().unwrap();
        let ev = AuditEvent {
            id: uuid::Uuid::new_v4(),
            session_id: "sess-agent".into(),
            conversation_id: Some("conv-agent".into()),
            action_type: "llm".into(),
            actor: "coder".into(),
            ..AuditEvent::default()
        };
        // A single-event conversation never hits the ON CONFLICT path, so the
        // initial INSERT must record the correct distinct-agent count.
        ingot.record_timeline_event(&ev).unwrap();
        let rollups = ingot.recent_conversations(10).unwrap();
        assert_eq!(rollups.len(), 1);
        assert_eq!(rollups[0].agent_count, 1);
    }

    #[test]
    fn prune_old_sessions_with_huge_days_does_not_overflow() {
        let ingot = Ingot::open_in_memory().unwrap();
        // A gigantic retention window must not overflow the day->micros
        // multiply (which would panic in debug or wrap to a bogus cutoff);
        // it should clamp and prune nothing.
        let deleted = ingot.prune_old_sessions(u64::MAX).unwrap();
        assert_eq!(deleted, 0);
        let deleted_large = ingot.prune_old_sessions(1_000_000_000_000_000).unwrap();
        assert_eq!(deleted_large, 0);
    }

    #[test]
    fn apply_migration_batch_runs_all_statements_after_idempotent_error() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        // Pre-create `demo` so the first statement of the batch fails with an
        // ignorable "already exists" error.
        conn.execute_batch("CREATE TABLE demo (a INTEGER);")
            .unwrap();

        // First statement errors idempotently; the second one matters and must
        // still run. A whole-batch execute would abort at the first error and
        // leave column `b` unapplied.
        let sql = "CREATE TABLE demo (a INTEGER); ALTER TABLE demo ADD COLUMN b INTEGER;";
        apply_migration_batch(&conn, 999, sql).unwrap();

        let has_b: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('demo') WHERE name = 'b'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            has_b, 1,
            "later statement must run even after an earlier idempotent error"
        );
    }

    #[test]
    fn apply_migration_batch_rolls_back_on_genuine_error() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE demo (a INTEGER);")
            .unwrap();

        // Second statement is a genuine error (unknown table): the batch must
        // roll back and return Err rather than half-apply the first statement.
        let sql = "ALTER TABLE demo ADD COLUMN b INTEGER; ALTER TABLE nope ADD COLUMN c INTEGER;";
        let res = apply_migration_batch(&conn, 998, sql);
        assert!(res.is_err(), "genuine error must propagate");

        let has_b: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('demo') WHERE name = 'b'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            has_b, 0,
            "the whole batch must roll back on a genuine error"
        );
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
