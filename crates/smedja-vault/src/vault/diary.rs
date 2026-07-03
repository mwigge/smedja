//! Diary storage: append-only role-scoped notes.

use super::{now_secs, Vault};
use crate::error::VaultError;

/// A single diary entry stored in the vault.
#[derive(Debug, Clone, PartialEq)]
pub struct DiaryEntry {
    /// Auto-incremented row identifier.
    pub id: i64,
    /// Role that wrote the diary entry (e.g. `"coder"`, `"reviewer"`).
    pub role: String,
    /// Free-text body of the diary entry.
    pub entry: String,
    /// Unix timestamp (seconds since epoch) when the entry was created.
    pub created_at: f64,
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diary_write_and_read() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault.diary_write("coder", "first entry").unwrap();
        vault.diary_write("coder", "second entry").unwrap();

        let entries = vault.diary_read("coder").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, "coder");
        assert_eq!(entries[0].entry, "first entry");
        assert_eq!(entries[1].entry, "second entry");
        assert!(
            entries[0].created_at <= entries[1].created_at,
            "entries must be returned in ascending created_at order"
        );
    }
}
