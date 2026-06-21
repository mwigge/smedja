//! [`Vault`] — vector KV cold store backed by `SQLite`.

use crate::error::VaultError;
use crate::similarity::cosine_sim;

/// A single entry stored in the vault.
#[derive(Debug, Clone, PartialEq)]
pub struct VaultEntry {
    /// Unique identifier for the entry.
    pub id: String,
    /// Embedding vector stored as raw `f32` components.
    pub embedding: Vec<f32>,
    /// Arbitrary JSON payload associated with the entry.
    pub payload: serde_json::Value,
}

/// A single result returned by [`Vault::query`].
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    /// Identifier matching a [`VaultEntry::id`].
    pub id: String,
    /// Cosine similarity score in `[0.0, 1.0]`.
    pub score: f32,
    /// The payload from the matching [`VaultEntry`].
    pub payload: serde_json::Value,
}

/// Vector KV cold store.
///
/// Embeddings are stored in `SQLite` as little-endian `f32` BLOBs. Retrieval
/// performs a full scan and returns the top-K entries by cosine similarity.
///
/// All operations are synchronous; callers inside an async runtime should use
/// [`tokio::task::spawn_blocking`] to avoid blocking the executor thread.
pub struct Vault {
    conn: rusqlite::Connection,
}

impl Vault {
    /// Opens or creates a vault database at `path`.
    ///
    /// Runs schema bootstrap on every open (idempotent via `CREATE TABLE IF NOT EXISTS`).
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the database cannot be opened or the
    /// schema bootstrap fails.
    pub fn open(path: &std::path::Path) -> Result<Self, VaultError> {
        let conn = rusqlite::Connection::open(path)?;
        let vault = Self { conn };
        vault.migrate()?;
        Ok(vault)
    }

    /// Opens an in-memory vault.
    ///
    /// Useful for tests and ephemeral sessions where durability is not required.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the in-memory connection cannot be
    /// established or the schema bootstrap fails.
    pub fn open_in_memory() -> Result<Self, VaultError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let vault = Self { conn };
        vault.migrate()?;
        Ok(vault)
    }

    /// Inserts or replaces a [`VaultEntry`] (upsert by `id`).
    ///
    /// If an entry with the same `id` already exists it is overwritten.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the database write fails, or
    /// [`VaultError::Json`] if `entry.payload` cannot be serialised.
    pub fn upsert(&mut self, entry: &VaultEntry) -> Result<(), VaultError> {
        let embedding_bytes = bytemuck::cast_slice::<f32, u8>(&entry.embedding);
        let payload_str = serde_json::to_string(&entry.payload)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO vault_entries (id, embedding, payload) VALUES (?1, ?2, ?3)",
            rusqlite::params![entry.id, embedding_bytes, payload_str],
        )?;
        Ok(())
    }

    /// Returns the top-K entries by cosine similarity to `query_embedding`.
    ///
    /// Performs a full table scan; suitable for cold stores with fewer than
    /// ~10,000 entries. Returns fewer than `k` results when the vault contains
    /// fewer entries. Returns an empty `Vec` when the vault is empty.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if reading any row fails, [`VaultError::Json`]
    /// if a stored payload cannot be deserialised, or
    /// [`VaultError::DimensionMismatch`] if any stored embedding has a different
    /// number of components than `query_embedding`.
    #[must_use = "the query result is the entire purpose of calling this function"]
    pub fn query(&self, query_embedding: &[f32], k: usize) -> Result<Vec<QueryResult>, VaultError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, embedding, payload FROM vault_entries")?;

        let mut scored: Vec<(f32, String, serde_json::Value)> = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let bytes: Vec<u8> = row.get(1)?;
                let payload_str: String = row.get(2)?;
                Ok((id, bytes, payload_str))
            })?
            .map(|result| {
                let (id, bytes, payload_str) = result?;

                // Safety invariant: bytes were written by `bytemuck::cast_slice::<f32, u8>`,
                // so length is always a multiple of 4 and alignment is satisfied.
                let stored: &[f32] = bytemuck::cast_slice::<u8, f32>(&bytes);

                if !query_embedding.is_empty()
                    && !stored.is_empty()
                    && stored.len() != query_embedding.len()
                {
                    return Err(VaultError::DimensionMismatch {
                        expected: query_embedding.len(),
                        got: stored.len(),
                    });
                }

                let score = cosine_sim(query_embedding, stored);
                let payload: serde_json::Value = serde_json::from_str(&payload_str)?;
                Ok((score, id, payload))
            })
            .collect::<Result<_, VaultError>>()?;

        // Sort descending by score, then take the top K.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);

        let results = scored
            .into_iter()
            .map(|(score, id, payload)| QueryResult { id, score, payload })
            .collect();
        Ok(results)
    }

    /// Removes the entry identified by `id`.
    ///
    /// This is a no-op when no entry with that `id` exists.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the database write fails.
    pub fn remove(&mut self, id: &str) -> Result<(), VaultError> {
        self.conn.execute(
            "DELETE FROM vault_entries WHERE id = ?1",
            rusqlite::params![id],
        )?;
        Ok(())
    }

    /// Returns the total number of entries stored in the vault.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the count query fails.
    #[must_use = "check the Result and use the returned count"]
    pub fn count(&self) -> Result<usize, VaultError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM vault_entries", [], |row| row.get(0))?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// Applies the `vault_entries` schema (idempotent).
    fn migrate(&self) -> Result<(), VaultError> {
        self.conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            CREATE TABLE IF NOT EXISTS vault_entries (
                id        TEXT PRIMARY KEY,
                embedding BLOB NOT NULL,
                payload   TEXT NOT NULL
            );
            ",
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(id: &str, embedding: Vec<f32>) -> VaultEntry {
        VaultEntry {
            id: id.to_string(),
            embedding,
            payload: json!({ "turn": id }),
        }
    }

    // ── basic CRUD ────────────────────────────────────────────────────────────

    #[test]
    fn upsert_and_count() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault.upsert(&entry("a", vec![1.0, 0.0])).unwrap();
        vault.upsert(&entry("b", vec![0.0, 1.0])).unwrap();
        assert_eq!(vault.count().unwrap(), 2);
    }

    #[test]
    fn upsert_replaces_existing() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault.upsert(&entry("a", vec![1.0, 0.0])).unwrap();
        vault.upsert(&entry("a", vec![0.5, 0.5])).unwrap();
        assert_eq!(vault.count().unwrap(), 1);
    }

    #[test]
    fn remove_decrements_count() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault.upsert(&entry("a", vec![1.0, 0.0])).unwrap();
        vault.remove("a").unwrap();
        assert_eq!(vault.count().unwrap(), 0);
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let mut vault = Vault::open_in_memory().unwrap();
        // Should not panic or return an error.
        vault.remove("does-not-exist").unwrap();
        assert_eq!(vault.count().unwrap(), 0);
    }

    // ── query ────────────────────────────────────────────────────────────────

    #[test]
    fn query_returns_most_similar() {
        let mut vault = Vault::open_in_memory().unwrap();
        // [1,0] is the query; [1,0] should score 1.0, [0,1] scores 0.0,
        // [0.6, 0.8] is intermediate.
        vault.upsert(&entry("exact", vec![1.0_f32, 0.0])).unwrap();
        vault.upsert(&entry("ortho", vec![0.0_f32, 1.0])).unwrap();
        vault.upsert(&entry("close", vec![0.6_f32, 0.8])).unwrap();

        let results = vault.query(&[1.0_f32, 0.0], 3).unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(
            results[0].id, "exact",
            "top result must be the identical vector"
        );
        assert!(
            results[0].score > results[1].score,
            "results must be sorted descending by score"
        );
    }

    #[test]
    fn query_k_larger_than_entries() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault.upsert(&entry("a", vec![1.0, 0.0])).unwrap();
        vault.upsert(&entry("b", vec![0.0, 1.0])).unwrap();
        vault.upsert(&entry("c", vec![0.5, 0.5])).unwrap();

        let results = vault.query(&[1.0_f32, 0.0], 10).unwrap();
        assert_eq!(results.len(), 3, "must return all entries when k > count");
    }

    #[test]
    fn query_empty_vault() {
        let vault = Vault::open_in_memory().unwrap();
        let results = vault.query(&[1.0_f32, 0.0], 5).unwrap();
        assert!(
            results.is_empty(),
            "query on empty vault must return empty vec"
        );
    }
}
