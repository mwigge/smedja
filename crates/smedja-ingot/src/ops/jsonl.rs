//! JSONL export/import operations.

use crate::{AuditEvent, Ingot, IngotError, Task};
impl Ingot {
    // JSONL export / import --------------------------------------------------

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

#[cfg(test)]
mod tests {
    use crate::*;

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
}
