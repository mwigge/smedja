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
    /// Namespace grouping for the entry. Defaults to `"default"` when empty.
    pub namespace: String,
    /// Raw text content used for keyword boosting and deduplication.
    pub content: String,
    /// Optional path to the source file that produced this entry.
    pub source_file: Option<String>,
    /// Optional identifier of the agent or process that inserted this entry.
    pub added_by: Option<String>,
    /// Position of this chunk within its parent document.
    pub chunk_index: Option<i64>,
    /// Identifier of the parent entry when this entry is a chunk.
    pub parent_id: Option<String>,
    /// Unix timestamp (seconds since epoch) when the entry was created.
    ///
    /// Set to `0.0` on construction; [`Vault::insert`] fills in the current
    /// wall-clock time when the stored value is `0.0`.
    pub created_at: f64,
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

/// Identity of the embedding model whose vectors are stored in this vault.
///
/// Once set, [`Vault::insert`] rejects embeddings whose dimension does not
/// match `dimensions`.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedderIdentity {
    /// Name or identifier of the embedding model.
    pub model: String,
    /// Number of dimensions produced by the model.
    pub dimensions: usize,
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

/// A row read during the dedup scan in [`Vault::insert`].
struct DedupeRow {
    id: String,
    embedding: Vec<u8>,
    content_len: usize,
}

/// Returns the current Unix time as a floating-point number of seconds.
fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
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
             (id, embedding, payload, namespace, content, source_file, added_by, chunk_index, parent_id, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
             (id, embedding, payload, namespace, content, source_file, added_by, chunk_index, parent_id, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
            ],
        )?;
        Ok(())
    }

    /// Performs a hybrid search: cosine similarity + keyword boost + recency boost.
    ///
    /// For each entry in `namespace`:
    /// - Compute cosine similarity with `query_vec`.
    /// - Add a keyword boost: for each whitespace-split token in `query_text`,
    ///   count case-insensitive occurrences in `entry.content` and add `0.01` per
    ///   occurrence.
    /// - Add a recency boost of `0.1` when `entry.created_at > (now − 86400.0)`.
    ///
    /// Results are sorted descending by total score. The top `k` are returned.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] on a database failure or [`VaultError::Json`] if
    /// a stored payload cannot be deserialised.
    #[must_use = "the search result is the entire purpose of calling this function"]
    pub fn search(
        &self,
        query_vec: &[f32],
        query_text: &str,
        namespace: &str,
        k: usize,
    ) -> Result<Vec<VaultEntry>, VaultError> {
        let namespace = if namespace.is_empty() {
            "default"
        } else {
            namespace
        };

        let mut stmt = self.conn.prepare(
            "SELECT id, embedding, payload, namespace, content, source_file, added_by, \
             chunk_index, parent_id, created_at \
             FROM vault_entries WHERE namespace = ?1",
        )?;

        let now = now_secs();
        let terms: Vec<String> = query_text
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();

        let mut scored: Vec<(f32, VaultEntry)> = stmt
            .query_map(rusqlite::params![namespace], |row| {
                let id: String = row.get(0)?;
                let bytes: Vec<u8> = row.get(1)?;
                let payload_str: String = row.get(2)?;
                let ns: String = row.get(3)?;
                let content: String = row.get(4)?;
                let source_file: Option<String> = row.get(5)?;
                let added_by: Option<String> = row.get(6)?;
                let chunk_index: Option<i64> = row.get(7)?;
                let parent_id: Option<String> = row.get(8)?;
                let created_at: f64 = row.get(9)?;
                Ok((
                    id,
                    bytes,
                    payload_str,
                    ns,
                    content,
                    source_file,
                    added_by,
                    chunk_index,
                    parent_id,
                    created_at,
                ))
            })?
            .map(|result| {
                let (
                    id,
                    bytes,
                    payload_str,
                    ns,
                    content,
                    source_file,
                    added_by,
                    chunk_index,
                    parent_id,
                    created_at,
                ) = result?;

                let stored: &[f32] = bytemuck::cast_slice::<u8, f32>(&bytes);
                let cosine_score = cosine_sim(query_vec, stored);

                let content_lower = content.to_lowercase();
                let keyword_boost: f32 = terms
                    .iter()
                    .map(|term| {
                        let mut count = 0usize;
                        let mut start = 0;
                        while let Some(pos) = content_lower[start..].find(term.as_str()) {
                            count += 1;
                            start += pos + term.len();
                        }
                        #[allow(clippy::cast_precision_loss)]
                        // keyword match counts fit comfortably in f32
                        {
                            count as f32 * 0.01
                        }
                    })
                    .sum();

                let recency_boost = if created_at > (now - 86_400.0) {
                    0.1_f32
                } else {
                    0.0_f32
                };

                let total_score = cosine_score + keyword_boost + recency_boost;

                let payload: serde_json::Value = serde_json::from_str(&payload_str)?;

                let entry = VaultEntry {
                    id,
                    embedding: stored.to_vec(),
                    payload,
                    namespace: ns,
                    content,
                    source_file,
                    added_by,
                    chunk_index,
                    parent_id,
                    created_at,
                };

                Ok((total_score, entry))
            })
            .collect::<Result<_, VaultError>>()?;

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);

        Ok(scored.into_iter().map(|(_, e)| e).collect())
    }

    /// Appends a diary entry for `role`.
    ///
    /// The entry is timestamped with the current Unix time.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the database write fails.
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

    /// Stores the embedder identity, overwriting any previously stored value.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the database write fails.
    pub fn set_embedder_identity(&mut self, identity: &EmbedderIdentity) -> Result<(), VaultError> {
        let json = format!(
            r#"{{"model":"{}","dimensions":{}}}"#,
            identity.model, identity.dimensions
        );
        self.conn.execute(
            "INSERT OR REPLACE INTO vault_meta (id, meta_key, meta_val) VALUES (1, 'embedder', ?1)",
            rusqlite::params![json],
        )?;
        Ok(())
    }

    /// Returns the stored embedder identity, or `None` if none has been set.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the query fails, or [`VaultError::Json`] if
    /// the stored JSON is malformed.
    #[must_use = "check the Result and use the returned identity"]
    pub fn get_embedder_identity(&self) -> Result<Option<EmbedderIdentity>, VaultError> {
        let result: rusqlite::Result<String> = self.conn.query_row(
            "SELECT meta_val FROM vault_meta WHERE id = 1 AND meta_key = 'embedder'",
            [],
            |row| row.get(0),
        );
        match result {
            Ok(json_str) => {
                let v: serde_json::Value = serde_json::from_str(&json_str)?;
                let model = v["model"].as_str().unwrap_or_default().to_string();
                let dimensions =
                    usize::try_from(v["dimensions"].as_u64().unwrap_or(0)).unwrap_or(0);
                Ok(Some(EmbedderIdentity { model, dimensions }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(VaultError::Db(e)),
        }
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

                // bytes were written by `bytemuck::cast_slice::<f32, u8>`,
                // so the length is always a multiple of 4.
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

    /// Applies the database schema (idempotent).
    ///
    /// Creates all tables and then attempts to add any columns that may be
    /// missing from databases created before this migration. The `ALTER TABLE`
    /// calls are executed with errors suppressed — `SQLite` returns an error for
    /// a duplicate column which is the expected outcome on a fully-migrated
    /// database.
    fn migrate(&self) -> Result<(), VaultError> {
        self.conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;

            CREATE TABLE IF NOT EXISTS vault_entries (
                id           TEXT PRIMARY KEY,
                embedding    BLOB NOT NULL,
                payload      TEXT NOT NULL,
                namespace    TEXT NOT NULL DEFAULT 'default',
                content      TEXT NOT NULL DEFAULT '',
                source_file  TEXT,
                added_by     TEXT,
                chunk_index  INTEGER,
                parent_id    TEXT,
                created_at   REAL NOT NULL DEFAULT 0.0
            );

            CREATE TABLE IF NOT EXISTS diary (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                role       TEXT NOT NULL,
                entry      TEXT NOT NULL,
                created_at REAL NOT NULL
            );

            CREATE TABLE IF NOT EXISTS vault_meta (
                id       INTEGER PRIMARY KEY,
                meta_key TEXT NOT NULL,
                meta_val TEXT NOT NULL
            );
            ",
        )?;

        // Idempotent column additions for databases created before this migration.
        // SQLite does not support "ADD COLUMN IF NOT EXISTS", so we swallow the
        // "duplicate column name" error that occurs on already-migrated databases.
        for col_def in &[
            "ALTER TABLE vault_entries ADD COLUMN namespace TEXT NOT NULL DEFAULT 'default'",
            "ALTER TABLE vault_entries ADD COLUMN content TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE vault_entries ADD COLUMN source_file TEXT",
            "ALTER TABLE vault_entries ADD COLUMN added_by TEXT",
            "ALTER TABLE vault_entries ADD COLUMN chunk_index INTEGER",
            "ALTER TABLE vault_entries ADD COLUMN parent_id TEXT",
            "ALTER TABLE vault_entries ADD COLUMN created_at REAL NOT NULL DEFAULT 0.0",
        ] {
            let _ = self.conn.execute(col_def, []); // "duplicate column" errors are expected
        }

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
            namespace: String::new(),
            content: String::new(),
            source_file: None,
            added_by: None,
            chunk_index: None,
            parent_id: None,
            created_at: 0.0,
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

    // ── namespace ────────────────────────────────────────────────────────────

    #[test]
    fn namespace_round_trip() {
        let mut vault = Vault::open_in_memory().unwrap();
        let mut e = entry("ns-entry", vec![1.0_f32, 0.0]);
        e.namespace = "agents".to_string();
        e.content = "agent context".to_string();
        vault.insert(&e).unwrap();

        let results = vault.search(&[1.0_f32, 0.0], "agent", "agents", 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "ns-entry");
        assert_eq!(results[0].namespace, "agents");
    }

    // ── diary ────────────────────────────────────────────────────────────────

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

    // ── dedup ────────────────────────────────────────────────────────────────

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

    // ── recency boost ─────────────────────────────────────────────────────────

    #[test]
    fn hybrid_recency_boost() {
        let mut vault = Vault::open_in_memory().unwrap();

        // Old entry: created_at far in the past (epoch 0 → no recency boost).
        let mut old = entry("old", vec![1.0_f32, 0.0]);
        old.content = "content".to_string();
        old.created_at = 1.0; // Unix time 1 — effectively ancient
        vault.upsert(&old).unwrap();

        // Recent entry: created_at is now → gets +0.1 recency boost.
        let mut recent = entry("recent", vec![1.0_f32, 0.0]);
        recent.content = "content".to_string();
        recent.created_at = now_secs();
        vault.upsert(&recent).unwrap();

        let results = vault.search(&[1.0_f32, 0.0], "", "default", 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].id, "recent",
            "recent entry must rank first due to recency boost"
        );
    }

    // ── embedder identity ─────────────────────────────────────────────────────

    #[test]
    fn embedder_mismatch_returns_error() {
        let mut vault = Vault::open_in_memory().unwrap();
        vault
            .set_embedder_identity(&EmbedderIdentity {
                model: "test-model".to_string(),
                dimensions: 2,
            })
            .unwrap();

        // Attempt to insert with 3-dimensional embedding → mismatch.
        let mut e = entry("x", vec![1.0_f32, 0.0, 0.0]);
        e.namespace = "default".to_string();
        let err = vault.insert(&e).unwrap_err();
        assert!(
            matches!(err, VaultError::EmbedderMismatch { .. }),
            "expected EmbedderMismatch, got {err:?}"
        );
    }

    #[test]
    fn embedder_identity_round_trip() {
        let mut vault = Vault::open_in_memory().unwrap();
        assert!(vault.get_embedder_identity().unwrap().is_none());

        vault
            .set_embedder_identity(&EmbedderIdentity {
                model: "text-embedding-3-small".to_string(),
                dimensions: 1536,
            })
            .unwrap();

        let stored = vault.get_embedder_identity().unwrap().unwrap();
        assert_eq!(stored.model, "text-embedding-3-small");
        assert_eq!(stored.dimensions, 1536);
    }
}
