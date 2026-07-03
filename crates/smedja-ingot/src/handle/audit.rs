//! Audit-event and conversation-timeline handle methods.

use crate::{AuditEvent, ConversationRollup, Ingot, IngotError, IngotHandle};
impl IngotHandle {
    // ── audit_events ────────────────────────────────────────────────────────

    /// Appends an [`AuditEvent`] to the audit log.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn insert_audit_event(&self, event: AuditEvent) -> Result<(), IngotError> {
        self.run_blocking(move |ig| ig.insert_audit_event(&event))
            .await
    }

    /// Returns all [`AuditEvent`]s for `session_id`, ordered by `ts` ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_audit_events(&self, session_id: &str) -> Result<Vec<AuditEvent>, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.list_audit_events(&session_id))
            .await
    }

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

    /// Returns all [`AuditEvent`]s, ordered by `ts` ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_all_audit_events(&self) -> Result<Vec<AuditEvent>, IngotError> {
        self.run_blocking(Ingot::list_all_audit_events).await
    }
}
