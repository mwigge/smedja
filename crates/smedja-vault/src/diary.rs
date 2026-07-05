//! Per-role diary log for the [`Vault`].
//!
//! An append-and-read log keyed by role, stored in the `diary` table. Distinct
//! from the embedding store: no vectors, no similarity, just timestamped text.

use crate::error::VaultError;
use crate::vault::{now_secs, DiaryEntry, Vault};

impl Vault {
    /// Appends a diary entry for `role`.
    ///
    /// The entry is timestamped with the current Unix time.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the database write fails.
    #[must_use = "check the Result to confirm the diary entry was written"]
    pub fn diary_write(&mut self, role: &str, entry: &str) -> Result<(), VaultError> {
        let created_at = now_secs();
        self.conn.execute(
            "INSERT INTO diary (role, entry, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![role, entry, created_at],
        )?;
        Ok(())
    }

    /// Returns all diary entries for `role`, ordered by `created_at` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the query fails.
    #[must_use = "check the Result and use the returned diary entries"]
    pub fn diary_read(&self, role: &str) -> Result<Vec<DiaryEntry>, VaultError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, role, entry, created_at FROM diary WHERE role = ?1 ORDER BY created_at ASC",
        )?;
        let entries = stmt
            .query_map(rusqlite::params![role], |row| {
                Ok(DiaryEntry {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    entry: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(entries)
    }
}
