//! JSONL export / import of tasks and their associated audit events.

use crate::error::IngotError;
use crate::{AuditEvent, Ingot, IngotHandle, Task};

impl Ingot {
    /// Exports tasks and their associated audit events as a JSONL stream.
    ///
    /// Each element in the returned [`Vec`] is a JSON object with a `"type"`
    /// field set to either `"task"` or `"audit_event"`, followed by all the
    /// fields of the respective struct.
    ///
    /// When `change` is `Some(name)`, only tasks whose `title` contains `name`
    /// (case-sensitive substring match) are exported together with their audit
    /// events.  When `change` is `None`, all tasks and all audit events are
    /// exported.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if any query fails, or [`IngotError::Json`]
    /// if serialisation fails.
    #[must_use = "check the Result and use the returned JSONL records"]
    pub fn export_jsonl(&self, change: Option<&str>) -> Result<Vec<serde_json::Value>, IngotError> {
        let tasks = self.list_tasks(None)?;
        let mut out: Vec<serde_json::Value> = Vec::new();

        for t in tasks {
            if let Some(name) = change {
                if !t.title.contains(name) {
                    continue;
                }
            }

            let mut task_obj = serde_json::to_value(&t)?;
            task_obj["type"] = serde_json::Value::String("task".to_owned());
            out.push(task_obj);

            if let Some(ref sid) = t.session_id {
                let events = self.list_audit_events(sid)?;
                for ev in events {
                    let mut ev_obj = serde_json::to_value(&ev)?;
                    ev_obj["type"] = serde_json::Value::String("audit_event".to_owned());
                    out.push(ev_obj);
                }
            }
        }

        Ok(out)
    }

    /// Imports tasks and audit events from a JSONL stream.
    ///
    /// Each element must be a JSON object with a `"type"` field of either
    /// `"task"` or `"audit_event"`.  Objects whose type field is unrecognised
    /// are silently skipped.  Rows that conflict with an existing primary key
    /// (`INSERT OR IGNORE`) are skipped so that re-running an import is
    /// idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Json`] if an object cannot be deserialised, or
    /// [`IngotError::Db`] if a database INSERT fails.
    #[must_use = "check the Result and inspect the returned import count"]
    pub fn import_jsonl(&self, records: &[serde_json::Value]) -> Result<usize, IngotError> {
        let mut imported = 0usize;
        for rec in records {
            match rec["type"].as_str() {
                Some("task") => {
                    let t: Task = serde_json::from_value(rec.clone())?;
                    let result = self.conn.execute(
                        "INSERT OR IGNORE INTO tasks \
                         (id, title, description, status, created_at, session_id, response) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        rusqlite::params![
                            t.id.to_string(),
                            t.title,
                            t.description,
                            t.status,
                            t.created_at.as_micros(),
                            t.session_id,
                            t.response,
                        ],
                    )?;
                    imported += result;
                }
                Some("audit_event") => {
                    let ev: AuditEvent = serde_json::from_value(rec.clone())?;
                    let result = self.conn.execute(
                        "INSERT OR IGNORE INTO audit_events \
                         (id, ts, session_id, turn_id, action_type, actor, tool_name, \
                          input_tok, output_tok, latency_ms, traceparent, tier, role_id, \
                          conversation_id, trace_id, span_id, parent_span_id, \
                          agent_name, operation_name, status, error_kind, error_count, \
                          tool_call_id) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, \
                                 ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
                        rusqlite::params![
                            ev.id.to_string(),
                            ev.ts.as_micros(),
                            ev.session_id,
                            ev.turn_id,
                            ev.action_type,
                            ev.actor,
                            ev.tool_name,
                            ev.input_tok,
                            ev.output_tok,
                            ev.latency_ms,
                            ev.traceparent,
                            ev.tier,
                            ev.role_id,
                            ev.conversation_id,
                            ev.trace_id,
                            ev.span_id,
                            ev.parent_span_id,
                            ev.agent_name,
                            ev.operation_name,
                            ev.status,
                            ev.error_kind,
                            ev.error_count,
                            ev.tool_call_id,
                        ],
                    )?;
                    imported += result;
                }
                _ => {
                    tracing::debug!(
                        record = ?rec,
                        "import_jsonl: skipping record with unrecognised type"
                    );
                }
            }
        }
        Ok(imported)
    }
}

impl IngotHandle {
    /// Exports tasks and their associated audit events as a JSONL stream.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] or [`IngotError::Json`] from the underlying
    /// export logic, or [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn export_jsonl(
        &self,
        change: Option<String>,
    ) -> Result<Vec<serde_json::Value>, IngotError> {
        self.run_blocking(move |ig| ig.export_jsonl(change.as_deref()))
            .await
    }

    /// Imports tasks and audit events from a JSONL stream.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Json`] or [`IngotError::Db`] from the underlying
    /// import logic, or [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn import_jsonl(&self, records: Vec<serde_json::Value>) -> Result<usize, IngotError> {
        self.run_blocking(move |ig| ig.import_jsonl(&records)).await
    }
}
