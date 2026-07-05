//! Per-session methodology lifecycle state — the spec-first gate's bookkeeping
//! and the per-session `--no-spec-gate` escape hatch.
//!
//! Stored in a dedicated `session_methodology` table keyed by session id rather
//! than as columns on `sessions`, so the spec-first lifecycle can evolve without
//! rippling through every `Session` construction site. A session with no row has
//! the all-false default (nothing recorded, gate engaged).

use crate::error::IngotError;
use crate::{Ingot, IngotHandle};

/// Spec-first lifecycle state for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MethodologyState {
    /// Whether a specification has been recorded for the active task.
    pub spec_recorded: bool,
    /// Whether the recorded specification has been approved.
    pub approval_recorded: bool,
    /// Whether the per-session methodology escape hatch is engaged. When `true`,
    /// both the spec-first check and the diff-level gates are bypassed.
    pub no_spec_gate: bool,
}

/// Returns the [`MethodologyState`] for `session_id`, or the all-false default
/// when no row exists yet.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn get(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<MethodologyState, IngotError> {
    let result = conn.query_row(
        "SELECT spec_recorded, approval_recorded, no_spec_gate \
         FROM session_methodology WHERE session_id = ?1",
        rusqlite::params![session_id],
        |row| {
            Ok(MethodologyState {
                spec_recorded: row.get::<_, i64>(0)? != 0,
                approval_recorded: row.get::<_, i64>(1)? != 0,
                no_spec_gate: row.get::<_, i64>(2)? != 0,
            })
        },
    );
    match result {
        Ok(s) => Ok(s),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(MethodologyState::default()),
        Err(e) => Err(IngotError::Db(e)),
    }
}

/// Sets the `spec_recorded` flag for `session_id`, inserting a row when absent.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the UPSERT fails.
pub(crate) fn set_spec_recorded(
    conn: &rusqlite::Connection,
    session_id: &str,
    value: bool,
) -> Result<(), IngotError> {
    conn.execute(
        "INSERT INTO session_methodology (session_id, spec_recorded, approval_recorded, no_spec_gate) \
         VALUES (?1, ?2, 0, 0) \
         ON CONFLICT(session_id) DO UPDATE SET spec_recorded = excluded.spec_recorded",
        rusqlite::params![session_id, i64::from(value)],
    )?;
    Ok(())
}

/// Sets the `approval_recorded` flag for `session_id`, inserting a row when absent.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the UPSERT fails.
pub(crate) fn set_approval_recorded(
    conn: &rusqlite::Connection,
    session_id: &str,
    value: bool,
) -> Result<(), IngotError> {
    conn.execute(
        "INSERT INTO session_methodology (session_id, spec_recorded, approval_recorded, no_spec_gate) \
         VALUES (?1, 0, ?2, 0) \
         ON CONFLICT(session_id) DO UPDATE SET approval_recorded = excluded.approval_recorded",
        rusqlite::params![session_id, i64::from(value)],
    )?;
    Ok(())
}

/// Sets the `no_spec_gate` escape-hatch flag for `session_id`, inserting a row
/// when absent.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the UPSERT fails.
pub(crate) fn set_no_spec_gate(
    conn: &rusqlite::Connection,
    session_id: &str,
    value: bool,
) -> Result<(), IngotError> {
    conn.execute(
        "INSERT INTO session_methodology (session_id, spec_recorded, approval_recorded, no_spec_gate) \
         VALUES (?1, 0, 0, ?2) \
         ON CONFLICT(session_id) DO UPDATE SET no_spec_gate = excluded.no_spec_gate",
        rusqlite::params![session_id, i64::from(value)],
    )?;
    Ok(())
}

impl Ingot {
    /// Returns the spec-first methodology state for `session_id`, or the
    /// all-false default when no row exists.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned methodology state"]
    pub fn get_methodology_state(&self, session_id: &str) -> Result<MethodologyState, IngotError> {
        get(&self.conn, session_id)
    }

    /// Sets the `spec_recorded` flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPSERT fails.
    #[must_use = "check the Result to confirm the flag was set"]
    pub fn set_spec_recorded(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        set_spec_recorded(&self.conn, session_id, value)
    }

    /// Sets the `approval_recorded` flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPSERT fails.
    #[must_use = "check the Result to confirm the flag was set"]
    pub fn set_approval_recorded(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        set_approval_recorded(&self.conn, session_id, value)
    }

    /// Sets the per-session `no_spec_gate` escape-hatch flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the UPSERT fails.
    #[must_use = "check the Result to confirm the flag was set"]
    pub fn set_no_spec_gate(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        set_no_spec_gate(&self.conn, session_id, value)
    }
}

impl IngotHandle {
    /// Returns the spec-first methodology state for `session_id`, or the
    /// all-false default when no row exists.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn get_methodology_state(
        &self,
        session_id: &str,
    ) -> Result<MethodologyState, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.get_methodology_state(&session_id))
            .await
    }

    /// Sets the `spec_recorded` flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn set_spec_recorded(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.set_spec_recorded(&session_id, value))
            .await
    }

    /// Sets the `approval_recorded` flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn set_approval_recorded(
        &self,
        session_id: &str,
        value: bool,
    ) -> Result<(), IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.set_approval_recorded(&session_id, value))
            .await
    }

    /// Sets the per-session `no_spec_gate` escape-hatch flag for `session_id`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying UPSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn set_no_spec_gate(&self, session_id: &str, value: bool) -> Result<(), IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.set_no_spec_gate(&session_id, value))
            .await
    }
}

#[cfg(test)]
mod tests {
    use crate::Ingot;

    #[test]
    fn unset_session_returns_all_false_default() {
        let ig = Ingot::open_in_memory().unwrap();
        let state = ig.get_methodology_state("sess-none").unwrap();
        assert!(!state.spec_recorded);
        assert!(!state.approval_recorded);
        assert!(!state.no_spec_gate);
    }

    #[test]
    fn spec_and_approval_round_trip() {
        let ig = Ingot::open_in_memory().unwrap();
        ig.set_spec_recorded("sess-1", true).unwrap();
        ig.set_approval_recorded("sess-1", true).unwrap();
        let state = ig.get_methodology_state("sess-1").unwrap();
        assert!(state.spec_recorded);
        assert!(state.approval_recorded);
        assert!(!state.no_spec_gate);
    }

    #[test]
    fn no_spec_gate_round_trip_independent_of_spec_flags() {
        let ig = Ingot::open_in_memory().unwrap();
        ig.set_no_spec_gate("sess-2", true).unwrap();
        let state = ig.get_methodology_state("sess-2").unwrap();
        assert!(state.no_spec_gate);
        assert!(
            !state.spec_recorded,
            "setting one flag must not set the others"
        );
    }

    #[test]
    fn flags_are_independently_updatable() {
        let ig = Ingot::open_in_memory().unwrap();
        ig.set_spec_recorded("sess-3", true).unwrap();
        ig.set_spec_recorded("sess-3", false).unwrap();
        ig.set_approval_recorded("sess-3", true).unwrap();
        let state = ig.get_methodology_state("sess-3").unwrap();
        assert!(!state.spec_recorded);
        assert!(state.approval_recorded);
    }
}
