//! Audit-event, timeline, and conversation-rollup operations.

use crate::{audit, now_epoch, AuditEvent, ConversationRollup, Ingot, IngotError};
impl Ingot {
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

    /// Returns the cumulative USD cost (microdollars) across all cost-ledger
    /// entries attributed to `change_name`.
    ///
    /// Returns `Ok(Microdollars::zero())` when no matching rows exist or when
    /// the `change_name` column is absent on a pre-migration database.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned microdollar total"]
    pub fn cost_usd_for_change(
        &self,
        change_name: &str,
    ) -> Result<smedja_types::Microdollars, IngotError> {
        let total: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM cost_ledger WHERE change_name = ?1",
            rusqlite::params![change_name],
            |row| row.get(0),
        )?;
        Ok(smedja_types::Microdollars::from_micros(total.max(0)))
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

#[cfg(test)]
mod tests {
    use crate::*;

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
}
