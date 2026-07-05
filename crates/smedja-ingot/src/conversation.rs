//! Multi-agent conversation timeline and rollup aggregation.
//!
//! Timeline events are audit events carrying a `conversation_id`; recording one
//! upserts the matching [`ConversationRollup`] counters atomically.

use crate::error::IngotError;
use crate::{AuditEvent, Ingot, IngotHandle};

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

impl Ingot {
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
        crate::audit::insert(&self.conn, event)?;

        let Some(ref conv_id) = event.conversation_id else {
            return Ok(());
        };

        #[allow(clippy::cast_possible_truncation)] // intentional: subsecond precision is not needed
        let now_secs = crate::migrations::now_epoch() as i64;
        let is_llm = i64::from(event.action_type == "llm");
        let is_tool = i64::from(event.action_type == "tool");
        let is_failure = i64::from(event.status.as_deref() == Some("error"));

        self.conn.execute(
            "INSERT INTO conversation_rollups \
             (conversation_id, started_at, last_seen_at, agent_count, \
              llm_call_count, tool_call_count, failure_count, \
              input_token_total, output_token_total) \
             VALUES (?1, ?2, ?2, \
                     (SELECT COUNT(DISTINCT COALESCE(agent_name, actor)) \
                      FROM audit_events WHERE conversation_id = ?1), \
                     ?3, ?4, ?5, ?6, ?7) \
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
        crate::audit::list_by_conversation(&self.conn, conversation_id)
    }

    /// Returns timeline events with `status = 'error'` for `conversation_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned events"]
    pub fn failed_events(&self, conversation_id: &str) -> Result<Vec<AuditEvent>, IngotError> {
        crate::audit::list_failed_by_conversation(&self.conn, conversation_id)
    }
}

impl IngotHandle {
    /// Persists a timeline event and upserts the matching [`ConversationRollup`].
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from either the INSERT or the upsert, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn record_timeline_event(&self, event: AuditEvent) -> Result<(), IngotError> {
        self.run_blocking(move |ig| ig.record_timeline_event(&event))
            .await
    }

    /// Returns the most recent `limit` [`ConversationRollup`]s by `last_seen_at` descending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn recent_conversations(
        &self,
        limit: u32,
    ) -> Result<Vec<ConversationRollup>, IngotError> {
        self.run_blocking(move |ig| ig.recent_conversations(limit))
            .await
    }

    /// Returns timeline events for `conversation_id`, ordered by `rowid` ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn conversation_timeline(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<AuditEvent>, IngotError> {
        let conversation_id = conversation_id.to_owned();
        self.run_blocking(move |ig| ig.conversation_timeline(&conversation_id))
            .await
    }

    /// Returns timeline events with `status = 'error'` for `conversation_id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn failed_events(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<AuditEvent>, IngotError> {
        let conversation_id = conversation_id.to_owned();
        self.run_blocking(move |ig| ig.failed_events(&conversation_id))
            .await
    }
}
