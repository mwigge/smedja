//! Session CRUD plus prune/vacuum maintenance operations.

use crate::{session, Ingot, IngotError, Session};
impl Ingot {
    // sessions ---------------------------------------------------------------

    /// Inserts a new [`Session`].
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails.
    #[must_use = "check the Result to confirm the session was created"]
    pub fn create_session(&self, session: &Session) -> Result<(), IngotError> {
        session::create(&self.conn, session)
    }

    /// Retrieves a [`Session`] by `id`, returning `None` when not found.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned session"]
    pub fn get_session(&self, id: &str) -> Result<Option<Session>, IngotError> {
        session::get(&self.conn, id)
    }

    /// Returns all [`Session`]s ordered by `created_at` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned sessions"]
    pub fn list_sessions(&self) -> Result<Vec<Session>, IngotError> {
        session::list(&self.conn)
    }

    /// Searches sessions where `title` or `workspace_root` contains `query` (case-insensitive).
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the matched sessions"]
    pub fn search_sessions(&self, query: &str) -> Result<Vec<Session>, IngotError> {
        session::search(&self.conn, query)
    }

    /// Deletes the session with the given `id`.
    ///
    /// Returns `true` if a row was deleted, `false` if no session with that `id`
    /// existed.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the DELETE fails.
    #[must_use = "check the Result to confirm the session was deleted"]
    pub fn delete_session(&self, id: &str) -> Result<bool, IngotError> {
        session::delete(&self.conn, id)
    }

    /// Deletes sessions with a terminal status (`complete`, `failed`, `orphaned`)
    /// whose `updated_at` timestamp is older than `older_than_days` days, then
    /// removes orphaned dependent rows from `checkpoints`, `cost_ledger`,
    /// `audit_events`, and `tasks`. Returns the number of sessions deleted.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError`] on database failure.
    pub fn prune_old_sessions(&self, older_than_days: u64) -> Result<usize, IngotError> {
        let micros_per_day: i64 = 86_400 * 1_000_000;
        let cutoff = smedja_types::Timestamp::now().as_micros()
            - i64::try_from(older_than_days).unwrap_or(i64::MAX) * micros_per_day;

        let deleted = {
            let tx = self.conn.unchecked_transaction()?;
            let n = tx.execute(
                "DELETE FROM sessions WHERE status IN ('complete','failed','orphaned') AND updated_at < ?1",
                rusqlite::params![cutoff],
            )?;
            for table in &["checkpoints", "cost_ledger", "audit_events"] {
                tx.execute(
                    &format!(
                        "DELETE FROM {table} WHERE session_id NOT IN (SELECT id FROM sessions)"
                    ),
                    [],
                )?;
            }
            tx.execute(
                "DELETE FROM tasks WHERE session_id IS NOT NULL AND session_id NOT IN (SELECT id FROM sessions)",
                [],
            )?;
            tx.commit()?;
            n
        };

        Ok(deleted)
    }

    /// Checkpoints the WAL and rebuilds the database file to reclaim space.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError`] on database failure.
    pub fn vacuum(&self) -> Result<(), IngotError> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        Ok(())
    }

    /// Updates the `status` of a session to `status` and records a new `updated_at`
    /// timestamp using the current Unix epoch.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the status was updated"]
    pub fn update_session_status(&self, id: &str, status: &str) -> Result<(), IngotError> {
        session::update_status(&self.conn, id, status, smedja_types::Timestamp::now())
    }

    /// Sets the `workspace_root` filesystem path for the session identified by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the workspace root was updated"]
    pub fn update_session_workspace_root(
        &self,
        session_id: &str,
        workspace_root: &str,
    ) -> Result<(), IngotError> {
        session::update_workspace_root(&self.conn, session_id, workspace_root)
    }

    /// Sets the `mode` field for the session identified by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the mode was updated"]
    pub fn update_session_mode(&self, session_id: &str, mode: &str) -> Result<(), IngotError> {
        session::update_mode(&self.conn, session_id, mode)
    }

    /// Sets the `model_override` field for the session identified by `session_id`.
    ///
    /// When set, `run_turn` uses this model name instead of the `SMEDJA_MODEL`
    /// environment variable.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the model override was updated"]
    pub fn update_session_model_override(
        &self,
        session_id: &str,
        model: &str,
    ) -> Result<(), IngotError> {
        session::update_model_override(&self.conn, session_id, model).map_err(IngotError::Db)
    }

    /// Sets the `runner_override` field for the session identified by `session_id`.
    ///
    /// When set, `run_turn` bypasses the assayer and routes directly to this runner
    /// (e.g. `"claude-cli"`, `"codex-cli"`, `"local"`).
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the runner override was updated"]
    pub fn update_session_runner_override(
        &self,
        session_id: &str,
        runner: &str,
    ) -> Result<(), IngotError> {
        session::update_runner_override(&self.conn, session_id, runner).map_err(IngotError::Db)
    }

    /// Links the session identified by `session_id` to a task by setting `task_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the task id was linked"]
    pub fn update_session_task_id(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<(), IngotError> {
        session::update_task_id(&self.conn, session_id, task_id)
    }

    /// Enables or disables the cowork gate for the session identified by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    #[must_use = "check the Result to confirm the cowork mode was updated"]
    pub fn update_session_cowork_mode(
        &self,
        session_id: &str,
        enabled: bool,
    ) -> Result<(), IngotError> {
        session::update_cowork_mode(&self.conn, session_id, enabled)
    }

    /// Sets the human-readable `title` for the session identified by `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPDATE fails.
    pub fn update_session_title(&self, session_id: &str, title: &str) -> Result<(), IngotError> {
        session::update_title(&self.conn, session_id, title)
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
