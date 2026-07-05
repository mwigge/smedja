//! Database maintenance: retention pruning and space reclamation.

use crate::error::IngotError;
use crate::{Ingot, IngotHandle};

impl Ingot {
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
        // Saturating arithmetic: a huge `older_than_days` clamps the offset (and
        // the resulting cutoff) instead of overflowing i64 and wrapping to a
        // wrong/negative cutoff. A clamped cutoff prunes nothing rather than
        // silently deleting rows.
        let offset = i64::try_from(older_than_days)
            .unwrap_or(i64::MAX)
            .saturating_mul(micros_per_day);
        let cutoff = smedja_types::Timestamp::now()
            .as_micros()
            .saturating_sub(offset);

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
}

impl IngotHandle {
    /// Deletes old terminated sessions and orphaned dependent rows.
    /// See [`Ingot::prune_old_sessions`].
    ///
    /// # Errors
    ///
    /// Returns [`IngotError`] on database failure.
    pub async fn prune_old_sessions(&self, older_than_days: u64) -> Result<usize, IngotError> {
        self.run_blocking(move |ig| ig.prune_old_sessions(older_than_days))
            .await
    }

    /// Checkpoints the WAL and rebuilds the database to reclaim space.
    /// See [`Ingot::vacuum`].
    ///
    /// # Errors
    ///
    /// Returns [`IngotError`] on database failure.
    pub async fn vacuum(&self) -> Result<(), IngotError> {
        self.run_blocking(Ingot::vacuum).await
    }
}
