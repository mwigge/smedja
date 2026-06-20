use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::IngotError;

/// An immutable audit record capturing a single agent action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Unique event identifier (UUID v4 stored as TEXT).
    pub id: Uuid,
    /// Unix epoch timestamp as `f64`.
    pub ts: f64,
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
          input_tok, output_tok, latency_ms, traceparent, tier) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            event.id.to_string(),
            event.ts,
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
        ],
    )?;
    Ok(())
}

/// Returns all [`AuditEvent`]s for the given `session_id`, ordered by `ts` ascending.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn list_by_session(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Vec<AuditEvent>, IngotError> {
    let mut stmt = conn.prepare(
        "SELECT id, ts, session_id, turn_id, action_type, actor, tool_name, \
                input_tok, output_tok, latency_ms, traceparent, tier \
         FROM audit_events \
         WHERE session_id = ?1 \
         ORDER BY ts ASC",
    )?;

    let rows = stmt.query_map(rusqlite::params![session_id], |row| {
        let id_str: String = row.get(0)?;
        let id = Uuid::parse_str(&id_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        Ok(AuditEvent {
            id,
            ts: row.get(1)?,
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
        })
    })?;

    rows.collect::<Result<Vec<_>, _>>().map_err(IngotError::Db)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ingot;

    fn sample_event(session_id: &str) -> AuditEvent {
        AuditEvent {
            id: Uuid::new_v4(),
            ts: 1_700_000_000.0,
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
        }
    }

    #[test]
    fn insert_then_list_returns_event() {
        let mut ingot = Ingot::open_in_memory().unwrap();
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
        let mut ingot = Ingot::open_in_memory().unwrap();
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
        let mut ingot = Ingot::open_in_memory().unwrap();
        let ev = AuditEvent {
            id: Uuid::new_v4(),
            ts: 1_700_000_001.0,
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
        };
        ingot.insert_audit_event(&ev).unwrap();
        let results = ingot.list_audit_events("s").unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].turn_id.is_none());
        assert_eq!(results[0].traceparent.as_deref(), Some("00-trace-span-01"));
    }
}
