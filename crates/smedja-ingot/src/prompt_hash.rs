//! Prompt hash governance — tracks content hashes of role prompts per change.
//!
//! Each record pairs a change name and role name with a content hash, providing
//! an immutable audit trail for prompt governance.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::IngotError;
use crate::{Ingot, IngotHandle};

/// A recorded prompt hash for a specific change and role.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptHashRecord {
    /// Unique record identifier (UUID v4 stored as TEXT).
    pub id: String,
    /// Name of the `OpenSpec` change this hash belongs to.
    pub change_name: String,
    /// Role name (e.g. `"implementer"`, `"reviewer"`).
    pub role: String,
    /// SHA-256 hex digest of the prompt content.
    pub hash: String,
    /// Unix epoch timestamp as `f64`.
    pub ts: f64,
}

/// Inserts a prompt hash record into the `prompt_hashes` table.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the INSERT fails.
pub(crate) fn save(
    conn: &rusqlite::Connection,
    change: &str,
    role: &str,
    hash: &str,
    ts: f64,
) -> Result<(), IngotError> {
    conn.execute(
        "INSERT INTO prompt_hashes (id, change_name, role, hash, ts) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![Uuid::new_v4().to_string(), change, role, hash, ts],
    )?;
    Ok(())
}

/// Returns the most recent hash for `(change, role)`, or `None` when not found.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn get_latest(
    conn: &rusqlite::Connection,
    change: &str,
    role: &str,
) -> Result<Option<String>, IngotError> {
    let mut stmt = conn.prepare(
        "SELECT hash FROM prompt_hashes \
         WHERE change_name = ?1 AND role = ?2 \
         ORDER BY ts DESC LIMIT 1",
    )?;
    let mut rows = stmt.query_map(rusqlite::params![change, role], |row| {
        row.get::<_, String>(0)
    })?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// Returns all prompt hash records for `change`, ordered by `ts` ascending.
///
/// # Errors
///
/// Returns [`IngotError::Db`] if the query fails.
pub(crate) fn list_by_change(
    conn: &rusqlite::Connection,
    change: &str,
) -> Result<Vec<PromptHashRecord>, IngotError> {
    let mut stmt = conn.prepare(
        "SELECT id, change_name, role, hash, ts FROM prompt_hashes \
         WHERE change_name = ?1 ORDER BY ts ASC",
    )?;
    let rows: Result<Vec<_>, _> = stmt
        .query_map(rusqlite::params![change], |row| {
            Ok(PromptHashRecord {
                id: row.get(0)?,
                change_name: row.get(1)?,
                role: row.get(2)?,
                hash: row.get(3)?,
                ts: row.get(4)?,
            })
        })?
        .collect();
    Ok(rows?)
}

impl Ingot {
    /// Records a prompt content hash for `(change, role)` at the current time.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the INSERT fails.
    #[must_use = "check the Result to confirm the hash was saved"]
    pub fn save_prompt_hash(&self, change: &str, role: &str, hash: &str) -> Result<(), IngotError> {
        save(
            &self.conn,
            change,
            role,
            hash,
            crate::migrations::now_epoch(),
        )
    }

    /// Returns the most recent prompt hash for `(change, role)`, or `None` when
    /// no record exists.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned hash"]
    pub fn get_prompt_hash(&self, change: &str, role: &str) -> Result<Option<String>, IngotError> {
        get_latest(&self.conn, change, role)
    }

    /// Returns all prompt hash records for `change`, ordered by `ts` ascending.
    ///
    /// # Errors
    ///
    /// Returns [`IngotError::Db`] if the query fails.
    #[must_use = "check the Result and inspect the returned records"]
    pub fn list_prompt_hashes(&self, change: &str) -> Result<Vec<PromptHashRecord>, IngotError> {
        list_by_change(&self.conn, change)
    }
}

impl IngotHandle {
    /// Records a prompt content hash for `(change, role)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn save_prompt_hash(
        &self,
        change: &str,
        role: &str,
        hash: &str,
    ) -> Result<(), IngotError> {
        let change = change.to_owned();
        let role = role.to_owned();
        let hash = hash.to_owned();
        self.run_blocking(move |ig| ig.save_prompt_hash(&change, &role, &hash))
            .await
    }

    /// Returns the most recent prompt hash for `(change, role)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn get_prompt_hash(
        &self,
        change: &str,
        role: &str,
    ) -> Result<Option<String>, IngotError> {
        let change = change.to_owned();
        let role = role.to_owned();
        self.run_blocking(move |ig| ig.get_prompt_hash(&change, &role))
            .await
    }

    /// Returns all prompt hash records for `change`, ordered by `ts` ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_prompt_hashes(
        &self,
        change: &str,
    ) -> Result<Vec<PromptHashRecord>, IngotError> {
        let change = change.to_owned();
        self.run_blocking(move |ig| ig.list_prompt_hashes(&change))
            .await
    }
}

#[cfg(test)]
mod tests {
    use crate::Ingot;

    #[test]
    fn save_and_get_prompt_hash() {
        let ig = Ingot::open_in_memory().unwrap();
        ig.save_prompt_hash("smedja", "implementer", "deadbeef")
            .unwrap();
        let h = ig.get_prompt_hash("smedja", "implementer").unwrap();
        assert_eq!(h, Some("deadbeef".to_owned()));
    }

    #[test]
    fn get_latest_returns_most_recent_hash() {
        let ig = Ingot::open_in_memory().unwrap();
        ig.save_prompt_hash("smedja", "implementer", "aaa").unwrap();
        ig.save_prompt_hash("smedja", "implementer", "bbb").unwrap();
        // bbb must win because it was inserted later (higher ts).
        let h = ig.get_prompt_hash("smedja", "implementer").unwrap();
        assert_eq!(h, Some("bbb".to_owned()));
    }

    #[test]
    fn list_prompt_hashes_for_change() {
        let ig = Ingot::open_in_memory().unwrap();
        ig.save_prompt_hash("smedja", "implementer", "aaa").unwrap();
        ig.save_prompt_hash("smedja", "reviewer", "bbb").unwrap();
        let hashes = ig.list_prompt_hashes("smedja").unwrap();
        assert_eq!(hashes.len(), 2);
    }

    #[test]
    fn list_prompt_hashes_isolates_by_change() {
        let ig = Ingot::open_in_memory().unwrap();
        ig.save_prompt_hash("alpha", "implementer", "aaa").unwrap();
        ig.save_prompt_hash("beta", "reviewer", "bbb").unwrap();
        let hashes = ig.list_prompt_hashes("alpha").unwrap();
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0].role, "implementer");
    }

    #[test]
    fn get_returns_none_for_unknown_change_role() {
        let ig = Ingot::open_in_memory().unwrap();
        let h = ig.get_prompt_hash("nonexistent", "role").unwrap();
        assert!(h.is_none());
    }
}
