//! Entry lifecycle: insert, upsert, remove, listing, and counting.

use super::{now_secs, resolve_dim, resolve_model_id, DedupeRow, SearchRow, Vault, VaultEntry};
use crate::error::VaultError;
use crate::similarity::cosine_sim;

impl Vault {
    /// Inserts an entry with deduplication and embedder identity validation.
    ///
    /// The rules applied in order are:
    ///
    /// 1. Normalise an empty `namespace` to `"default"`.
    /// 2. Check embedder identity: if a stored identity exists and `entry.embedding.len()`
    ///    does not match `stored.dimensions`, return [`VaultError::EmbedderMismatch`].
    /// 3. Scan same-namespace entries. When `cosine_sim > 0.85`:
    ///    - If the existing entry's content is at least as long as the incoming → discard
    ///      the incoming entry (return `Ok(())`).
    ///    - Otherwise → remove the existing entry and continue insertion.
    /// 4. Persist the incoming entry. If `entry.created_at == 0.0`, the current Unix
    ///    timestamp is used.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::EmbedderMismatch`] on dimension mismatch with the stored
    /// identity, [`VaultError::Db`] on a database failure, or [`VaultError::Json`] if
    /// the payload cannot be serialised.
    #[must_use = "check the Result to confirm the entry was persisted"]
    pub fn insert(&mut self, entry: &VaultEntry) -> Result<(), VaultError> {
        // 1. Normalise namespace.
        let namespace = if entry.namespace.is_empty() {
            "default"
        } else {
            &entry.namespace
        };

        // 2. Embedder identity check.
        if let Some(identity) = self.get_embedder_identity()? {
            if identity.dimensions != entry.embedding.len() {
                return Err(VaultError::EmbedderMismatch {
                    stored: format!("{}/{}", identity.model, identity.dimensions),
                    incoming: format!("?/{}", entry.embedding.len()),
                });
            }
        }

        // 3. Dedup scan within the same namespace.
        //    Load id, embedding bytes, and content length from the namespace.
        let rows: Vec<DedupeRow> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id, embedding, content FROM vault_entries WHERE namespace = ?1")?;
            let collected: Result<Vec<_>, rusqlite::Error> = stmt
                .query_map(rusqlite::params![namespace], |row| {
                    let id: String = row.get(0)?;
                    let bytes: Vec<u8> = row.get(1)?;
                    let content: String = row.get(2)?;
                    Ok(DedupeRow {
                        id,
                        embedding: bytes,
                        content_len: content.len(),
                    })
                })?
                .collect();
            // `stmt` is dropped here; `collected` owns all data.
            collected?
        };

        for row in rows {
            let stored: &[f32] = bytemuck::cast_slice::<u8, f32>(&row.embedding);
            let sim = cosine_sim(&entry.embedding, stored);
            if sim > 0.85 {
                if row.content_len >= entry.content.len() {
                    // Existing entry is at least as good — discard incoming.
                    return Ok(());
                }
                // Incoming is longer — remove the existing entry.
                self.conn.execute(
                    "DELETE FROM vault_entries WHERE id = ?1",
                    rusqlite::params![row.id],
                )?;
            }
        }

        // 4. Persist.
        let created_at = if entry.created_at == 0.0 {
            now_secs()
        } else {
            entry.created_at
        };

        let embedding_bytes = bytemuck::cast_slice::<f32, u8>(&entry.embedding);
        let payload_str = serde_json::to_string(&entry.payload)?;

        self.conn.execute(
            "INSERT OR REPLACE INTO vault_entries \
             (id, embedding, payload, namespace, content, source_file, added_by, chunk_index, parent_id, created_at, embedder_model_id, dim) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                entry.id,
                embedding_bytes,
                payload_str,
                namespace,
                entry.content,
                entry.source_file,
                entry.added_by,
                entry.chunk_index,
                entry.parent_id,
                created_at,
                entry.embedder_model_id,
                i64::try_from(entry.dim).unwrap_or(i64::MAX),
            ],
        )?;
        Ok(())
    }

    /// Inserts or replaces a [`VaultEntry`] (upsert by `id`).
    ///
    /// Legacy path — does not perform deduplication or embedder-identity checks.
    /// Prefer [`Vault::insert`] for new call sites.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the database write fails, or
    /// [`VaultError::Json`] if `entry.payload` cannot be serialised.
    #[must_use = "check the Result to confirm the upsert succeeded"]
    pub fn upsert(&mut self, entry: &VaultEntry) -> Result<(), VaultError> {
        let embedding_bytes = bytemuck::cast_slice::<f32, u8>(&entry.embedding);
        let payload_str = serde_json::to_string(&entry.payload)?;
        let namespace = if entry.namespace.is_empty() {
            "default"
        } else {
            &entry.namespace
        };
        self.conn.execute(
            "INSERT OR REPLACE INTO vault_entries \
             (id, embedding, payload, namespace, content, source_file, added_by, chunk_index, parent_id, created_at, embedder_model_id, dim) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                entry.id,
                embedding_bytes,
                payload_str,
                namespace,
                entry.content,
                entry.source_file,
                entry.added_by,
                entry.chunk_index,
                entry.parent_id,
                entry.created_at,
                entry.embedder_model_id,
                i64::try_from(entry.dim).unwrap_or(i64::MAX),
            ],
        )?;
        Ok(())
    }

    /// Returns every entry in `namespace` as a [`VaultEntry`], unranked.
    ///
    /// Unlike [`Vault::search`] this applies no scoring, no `k` limit, and no
    /// same-model filter — it is the read half of the re-embed/backfill path,
    /// which must visit every row regardless of its current model. An empty
    /// `namespace` is normalised to `"default"`.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] on a database failure or [`VaultError::Json`]
    /// if a stored payload cannot be deserialised.
    #[must_use = "the listed entries are the input to a re-embed pass"]
    pub fn list_namespace(&self, namespace: &str) -> Result<Vec<VaultEntry>, VaultError> {
        let namespace = if namespace.is_empty() {
            "default"
        } else {
            namespace
        };

        let mut stmt = self.conn.prepare(
            "SELECT id, embedding, payload, namespace, content, source_file, added_by, \
             chunk_index, parent_id, created_at, embedder_model_id, dim \
             FROM vault_entries WHERE namespace = ?1",
        )?;

        let rows: Vec<SearchRow> = stmt
            .query_map(rusqlite::params![namespace], |row| {
                Ok(SearchRow {
                    id: row.get(0)?,
                    bytes: row.get(1)?,
                    payload_str: row.get(2)?,
                    ns: row.get(3)?,
                    content: row.get(4)?,
                    source_file: row.get(5)?,
                    added_by: row.get(6)?,
                    chunk_index: row.get(7)?,
                    parent_id: row.get(8)?,
                    created_at: row.get(9)?,
                    model_id: row.get(10)?,
                    dim: row.get(11)?,
                })
            })?
            .collect::<Result<_, rusqlite::Error>>()?;

        rows.into_iter()
            .map(|row| {
                let embedder_model_id = resolve_model_id(row.model_id);
                let dim = resolve_dim(row.dim, row.bytes.len());
                let embedding = bytemuck::cast_slice::<u8, f32>(&row.bytes).to_vec();
                let payload: serde_json::Value = serde_json::from_str(&row.payload_str)?;
                Ok(VaultEntry {
                    id: row.id,
                    embedding,
                    payload,
                    namespace: row.ns,
                    content: row.content,
                    source_file: row.source_file,
                    added_by: row.added_by,
                    chunk_index: row.chunk_index,
                    parent_id: row.parent_id,
                    created_at: row.created_at,
                    embedder_model_id,
                    dim,
                })
            })
            .collect()
    }

    /// Returns every distinct namespace present in the vault.
    ///
    /// Used by the re-embed/backfill path to walk the whole vault when no single
    /// namespace is specified.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the query fails.
    #[must_use = "the namespace list is the input to a whole-vault re-embed"]
    pub fn distinct_namespaces(&self) -> Result<Vec<String>, VaultError> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT namespace FROM vault_entries")?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(names)
    }

    /// Removes the entry identified by `id`.
    ///
    /// This is a no-op when no entry with that `id` exists.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the database write fails.
    #[must_use = "check the Result to confirm the entry was removed"]
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

    /// Returns the number of entries in the given `namespace`.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the count query fails.
    #[must_use = "check the Result and use the returned count"]
    pub fn count_by_namespace(&self, namespace: &str) -> Result<usize, VaultError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM vault_entries WHERE namespace = ?1",
            rusqlite::params![namespace],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(n).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::super::entry;
    use super::*;

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

    #[test]
    fn dedup_drops_near_duplicate() {
        let mut vault = Vault::open_in_memory().unwrap();

        // Entry A with longer content.
        let mut a = entry("a", vec![1.0_f32, 0.0]);
        a.content = "this is a longer content string that wins the dedup race".to_string();
        vault.insert(&a).unwrap();
        assert_eq!(vault.count().unwrap(), 1);

        // Entry B with nearly identical embedding but shorter content → should be dropped.
        let mut b = entry("b", vec![0.9999_f32, 0.0141]);
        b.content = "short".to_string();
        vault.insert(&b).unwrap();

        // Only A remains.
        assert_eq!(vault.count().unwrap(), 1);
    }

    #[test]
    fn count_by_namespace_isolates_entries() {
        let mut vault = Vault::open_in_memory().unwrap();

        let mut e1 = entry("warm1", vec![1.0, 0.0]);
        e1.namespace = "warm".to_string();
        let mut e2 = entry("warm2", vec![0.5, 0.5]);
        e2.namespace = "warm".to_string();
        let mut e3 = entry("cold1", vec![0.0, 1.0]);
        e3.namespace = "default".to_string();

        vault.upsert(&e1).unwrap();
        vault.upsert(&e2).unwrap();
        vault.upsert(&e3).unwrap();

        assert_eq!(vault.count_by_namespace("warm").unwrap(), 2);
        assert_eq!(vault.count_by_namespace("default").unwrap(), 1);
        assert_eq!(vault.count_by_namespace("missing").unwrap(), 0);
        assert_eq!(vault.count().unwrap(), 3);
    }
}
