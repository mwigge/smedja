use serde::{Deserialize, Serialize};
use smedja_types::Timestamp;
use uuid::Uuid;

use crate::error::IngotError;

/// An immutable audit record capturing a single agent action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Unique event identifier (UUID v4 stored as TEXT).
    pub id: Uuid,
    /// Event timestamp (microseconds since the Unix epoch).
    pub ts: Timestamp,
    /// Owning session identifier.
    pub session_id: String,
    /// Optional turn identifier within the session.
    pub turn_id: Option<String>,
    /// Broad action category: `"tool_exec"`, `"approval"`, `"turn_start"`, `"turn_end"`.
    pub action_type: String,
    /// Role name or `"user"` that performed the action.
    pub actor: String,
    /// Tool name when `action_type` is `"tool_exec"`.
    pub tool_name: Option<String>,
    /// Input token count for the action.
    pub input_tok: i64,
    /// Output token count for the action.
    pub output_tok: i64,
    /// Wall-clock latency of the action in milliseconds.
    pub latency_ms: i64,
    /// W3C traceparent header value for distributed tracing correlation.
    pub traceparent: Option<String>,
    /// Model tier: `"local"`, `"fast"`, or `"deep"`.
    pub tier: Option<String>,
    /// Deterministic role identity — `SHA-256(loop_id + "-" + role_name)` as a UUID.
    ///
    /// `None` for events emitted outside a loop context (e.g. plain sessions).
    #[serde(default)]
    pub role_id: Option<String>,
    // ── section 2 timeline columns ───────────────────────────────────────────
    /// Conversation grouping identifier (groups multiple turns / agents).
    #[serde(default)]
    pub conversation_id: Option<String>,
    /// W3C `trace-id` component extracted from `traceparent`.
    #[serde(default)]
    pub trace_id: Option<String>,
    /// W3C `parent-id` (span-id) component extracted from `traceparent`.
    #[serde(default)]
    pub span_id: Option<String>,
    /// Parent span identifier for distributed tracing correlation.
    #[serde(default)]
    pub parent_span_id: Option<String>,
    /// Agent name that produced this event.
    #[serde(default)]
    pub agent_name: Option<String>,
    /// High-level operation label (e.g. `"chat"`, `"tool_call"`, `"embed"`).
    #[serde(default)]
    pub operation_name: Option<String>,
    /// Terminal status of the operation: `"ok"` or `"error"`.
    #[serde(default)]
    pub status: Option<String>,
    /// Error category when `status = "error"`.
    #[serde(default)]
    pub error_kind: Option<String>,
    /// Number of errors that occurred during the operation.
    #[serde(default)]
    pub error_count: Option<i64>,
    /// Tool-call identifier for correlation with tool responses.
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

impl Default for AuditEvent {
    fn default() -> Self {
        Self {
            id: Uuid::default(),
            ts: Timestamp::from_micros(0),
            session_id: String::new(),
            turn_id: None,
            action_type: String::new(),
            actor: String::new(),
            tool_name: None,
            input_tok: 0,
            output_tok: 0,
            latency_ms: 0,
            traceparent: None,
            tier: None,
            role_id: None,
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            agent_name: None,
            operation_name: None,
            status: None,
            error_kind: None,
            error_count: None,
            tool_call_id: None,
        }
    }
}

/// Inserts an [`AuditEvent`] into the `audit_events` table.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the INSERT fails (e.g. duplicate primary key or
/// constraint violation).
pub(crate) fn insert(conn: &rusqlite::Connection, event: &AuditEvent) -> Result<(), IngotError> {
    conn.execute(
        "INSERT INTO audit_events \
         (id, ts, session_id, turn_id, action_type, actor, tool_name, \
          input_tok, output_tok, latency_ms, traceparent, tier, role_id, \
          conversation_id, trace_id, span_id, parent_span_id, \
          agent_name, operation_name, status, error_kind, error_count, tool_call_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, \
                 ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
        rusqlite::params![
            event.id.to_string(),
            event.ts.as_micros(),
            event.session_id,
            event.turn_id,
            event.action_type,
            event.actor,
            event.tool_name,
            event.input_tok,
            event.output_tok,
            event.latency_ms,
            event.traceparent,
            event.tier,
            event.role_id,
            event.conversation_id,
            event.trace_id,
            event.span_id,
            event.parent_span_id,
            event.agent_name,
            event.operation_name,
            event.status,
            event.error_kind,
            event.error_count,
            event.tool_call_id,
        ],
    )?;
    Ok(())
}

/// Maps a rusqlite row to an [`AuditEvent`], reading all 23 columns in SELECT order.
fn row_to_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditEvent> {
    let id_str: String = row.get(0)?;
    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(AuditEvent {
        id,
        ts: Timestamp::from_micros(crate::read_micros(row, 1)?),
        session_id: row.get(2)?,
        turn_id: row.get(3)?,
        action_type: row.get(4)?,
        actor: row.get(5)?,
        tool_name: row.get(6)?,
        input_tok: row.get(7)?,
        output_tok: row.get(8)?,
        latency_ms: row.get(9)?,
        traceparent: row.get(10)?,
        tier: row.get(11)?,
        role_id: row.get(12)?,
        conversation_id: row.get(13)?,
        trace_id: row.get(14)?,
        span_id: row.get(15)?,
        parent_span_id: row.get(16)?,
        agent_name: row.get(17)?,
        operation_name: row.get(18)?,
        status: row.get(19)?,
        error_kind: row.get(20)?,
        error_count: row.get(21)?,
        tool_call_id: row.get(22)?,
    })
}

/// Column list shared by all SELECT statements in this module.
const SELECT_COLS: &str = "id, ts, session_id, turn_id, action_type, actor, tool_name, \
     input_tok, output_tok, latency_ms, traceparent, tier, role_id, \
     conversation_id, trace_id, span_id, parent_span_id, \
     agent_name, operation_name, status, error_kind, error_count, tool_call_id";

/// Returns all [`AuditEvent`]s for the given `session_id`, ordered by `ts` ascending.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn list_by_session(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Vec<AuditEvent>, IngotError> {
    let sql =
        format!("SELECT {SELECT_COLS} FROM audit_events WHERE session_id = ?1 ORDER BY ts ASC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![session_id], row_to_event)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(IngotError::Db)
}

/// Returns all [`AuditEvent`]s, ordered by `ts` ascending.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn list_all(conn: &rusqlite::Connection) -> Result<Vec<AuditEvent>, IngotError> {
    let sql = format!("SELECT {SELECT_COLS} FROM audit_events ORDER BY ts ASC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_event)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(IngotError::Db)
}

/// Returns all [`AuditEvent`]s for `conversation_id`, ordered by `rowid` ascending.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn list_by_conversation(
    conn: &rusqlite::Connection,
    conversation_id: &str,
) -> Result<Vec<AuditEvent>, IngotError> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM audit_events \
         WHERE conversation_id = ?1 ORDER BY rowid ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![conversation_id], row_to_event)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(IngotError::Db)
}

/// Returns [`AuditEvent`]s with `status = 'error'` for `conversation_id`.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn list_failed_by_conversation(
    conn: &rusqlite::Connection,
    conversation_id: &str,
) -> Result<Vec<AuditEvent>, IngotError> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM audit_events \
         WHERE conversation_id = ?1 AND status = 'error' ORDER BY rowid ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![conversation_id], row_to_event)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(IngotError::Db)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ingot;

    fn sample_event(session_id: &str) -> AuditEvent {
        AuditEvent {
            id: Uuid::new_v4(),
            ts: Timestamp::from_secs_f64(1_700_000_000.0),
            session_id: session_id.to_string(),
            turn_id: Some("turn-1".to_string()),
            action_type: "tool_exec".to_string(),
            actor: "coder-rust".to_string(),
            tool_name: Some("bash".to_string()),
            input_tok: 100,
            output_tok: 50,
            latency_ms: 123,
            traceparent: None,
            tier: Some("fast".to_string()),
            role_id: None,
            conversation_id: None,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            agent_name: None,
            operation_name: None,
            status: None,
            error_kind: None,
            error_count: None,
            tool_call_id: None,
        }
    }

    #[test]
    fn insert_then_list_returns_event() {
        let ingot = Ingot::open_in_memory().unwrap();
        let ev = sample_event("session-abc");
        ingot.insert_audit_event(&ev).unwrap();

        let results = ingot.list_audit_events("session-abc").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, ev.id);
        assert_eq!(results[0].actor, "coder-rust");
        assert_eq!(results[0].input_tok, 100);
        assert_eq!(results[0].tier.as_deref(), Some("fast"));
    }

    #[test]
    fn list_filters_by_session_id() {
        let ingot = Ingot::open_in_memory().unwrap();
        ingot
            .insert_audit_event(&sample_event("session-1"))
            .unwrap();
        ingot
            .insert_audit_event(&sample_event("session-2"))
            .unwrap();

        let results = ingot.list_audit_events("session-1").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_id, "session-1");
    }

    #[test]
    fn list_empty_session_returns_empty_vec() {
        let ingot = Ingot::open_in_memory().unwrap();
        let results = ingot.list_audit_events("no-such-session").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn nullable_fields_round_trip() {
        let ingot = Ingot::open_in_memory().unwrap();
        let ev = AuditEvent {
            id: Uuid::new_v4(),
            ts: Timestamp::from_secs_f64(1_700_000_001.0),
            session_id: "s".to_string(),
            turn_id: None,
            action_type: "turn_start".to_string(),
            actor: "user".to_string(),
            tool_name: None,
            input_tok: 0,
            output_tok: 0,
            latency_ms: 0,
            traceparent: Some("00-trace-span-01".to_string()),
            tier: None,
            role_id: Some("test-role-id".to_string()),
            conversation_id: Some("conv-nullable".to_string()),
            trace_id: Some("tid-1".to_string()),
            span_id: Some("sid-1".to_string()),
            parent_span_id: None,
            agent_name: None,
            operation_name: None,
            status: None,
            error_kind: None,
            error_count: None,
            tool_call_id: None,
        };
        ingot.insert_audit_event(&ev).unwrap();
        let results = ingot.list_audit_events("s").unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].turn_id.is_none());
        assert_eq!(results[0].traceparent.as_deref(), Some("00-trace-span-01"));
        assert_eq!(results[0].role_id.as_deref(), Some("test-role-id"));
        assert_eq!(results[0].conversation_id.as_deref(), Some("conv-nullable"));
        assert_eq!(results[0].trace_id.as_deref(), Some("tid-1"));
    }
}
