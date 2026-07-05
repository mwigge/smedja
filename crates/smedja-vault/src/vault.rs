//! [`Vault`] — vector KV cold store backed by `SQLite`.

use crate::error::VaultError;
use crate::similarity::cosine_sim;

/// Default model identifier assumed for rows that predate per-row model tagging.
///
/// Legacy rows were all produced by the FNV-1a bag-of-words embedder, so a row
/// whose `embedder_model_id` column is absent or NULL reads back as this id.
pub const LEGACY_MODEL_ID: &str = "fnv-bow-128";

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
    /// Identifier of the embedding model that produced [`VaultEntry::embedding`].
    ///
    /// Persisted alongside the embedding so [`Vault::search`]/[`Vault::query`]
    /// compare only same-model vectors. Legacy rows lacking this column read
    /// back as [`LEGACY_MODEL_ID`].
    pub embedder_model_id: String,
    /// Dimension of [`VaultEntry::embedding`] as reported by its producing model.
    ///
    /// Legacy rows lacking this column derive it from the stored BLOB length
    /// divided by four (the byte width of an `f32`).
    pub dim: usize,
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

/// A row read during [`Vault::query`] before same-model filtering and scoring.
struct QueryRow {
    id: String,
    bytes: Vec<u8>,
    payload_str: String,
    model_id: Option<String>,
    dim: Option<i64>,
}

/// A fully-hydrated row read during [`Vault::search`].
///
/// The whole row set is materialised before scoring so the prepared statement is
/// dropped before the same-model filter and cosine math run.
struct SearchRow {
    id: String,
    bytes: Vec<u8>,
    payload_str: String,
    ns: String,
    content: String,
    source_file: Option<String>,
    added_by: Option<String>,
    chunk_index: Option<i64>,
    parent_id: Option<String>,
    created_at: f64,
    model_id: Option<String>,
    dim: Option<i64>,
}

/// Returns the current Unix time as a floating-point number of seconds.
fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Resolves the stored `embedder_model_id`, defaulting a legacy NULL to
/// [`LEGACY_MODEL_ID`].
fn resolve_model_id(stored: Option<String>) -> String {
    stored.unwrap_or_else(|| LEGACY_MODEL_ID.to_owned())
}

/// Resolves the stored `dim`, deriving a legacy NULL from the BLOB byte length.
///
/// Embeddings are written as little-endian `f32` BLOBs, so a legacy row's
/// dimension is its byte length divided by four.
fn resolve_dim(stored: Option<i64>, embedding_bytes_len: usize) -> usize {
    match stored {
        Some(d) if d >= 0 => usize::try_from(d).unwrap_or(embedding_bytes_len / 4),
        _ => embedding_bytes_len / 4,
    }
}

/// Decodes a little-endian `f32` embedding BLOB.
///
/// Returns `None` when `bytes.len()` is not a multiple of four — the signature
/// of a truncated, legacy, or externally-corrupted row. Callers skip such rows
/// so a single malformed BLOB cannot turn a full-scan `query`/`search` into a
/// store-wide panic. Decoding byte-by-byte (rather than `bytemuck::cast_slice`)
/// also sidesteps the alignment requirement that `cast_slice` imposes on the
/// borrowed `&[u8]`.
fn decode_embedding(bytes: &[u8]) -> Option<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
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
    #[must_use = "check the Result; a failed open means the vault is unavailable"]
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
    #[must_use = "check the Result; a failed open means the in-memory vault is unavailable"]
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
            let Some(stored) = decode_embedding(&row.embedding) else {
                tracing::warn!(
                    id = %row.id,
                    "vault: skipping dedup candidate with malformed embedding blob"
                );
                continue;
            };
            let sim = cosine_sim(&entry.embedding, &stored);
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

    /// Performs a hybrid search: cosine similarity + keyword boost + recency boost.
    ///
    /// Only rows produced by the same model as the query participate: a row whose
    /// resolved `embedder_model_id` ≠ `query_model_id` or resolved `dim` ≠
    /// `query_dim` is skipped before any cosine comparison — never an error.
    /// Comparing vectors from different models is meaningless, so mismatched rows
    /// are excluded from ranking rather than silently compared (or crashed).
    ///
    /// For each surviving entry in `namespace`:
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
    #[allow(clippy::too_many_lines)] // single hybrid-scoring scan; splitting it would obscure the flow
    pub fn search(
        &self,
        query_vec: &[f32],
        query_text: &str,
        namespace: &str,
        k: usize,
        query_model_id: &str,
        query_dim: usize,
    ) -> Result<Vec<VaultEntry>, VaultError> {
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

        let now = now_secs();
        let terms: Vec<String> = query_text
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();

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

        let mut scored: Vec<(f32, VaultEntry)> = rows
            .into_iter()
            // Same-model-only: skip any row whose resolved model/dim differs from
            // the query's before it ever reaches `cosine_sim`.
            .filter(|row| {
                resolve_model_id(row.model_id.clone()) == query_model_id
                    && resolve_dim(row.dim, row.bytes.len()) == query_dim
            })
            .filter_map(|row| {
                let SearchRow {
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
                    model_id,
                    dim,
                } = row;

                let embedder_model_id = resolve_model_id(model_id);
                let resolved_dim = resolve_dim(dim, bytes.len());
                let Some(stored) = decode_embedding(&bytes) else {
                    tracing::warn!(id = %id, "vault: skipping search row with malformed embedding blob");
                    return None;
                };
                let cosine_score = cosine_sim(query_vec, &stored);

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

                let payload: serde_json::Value = match serde_json::from_str(&payload_str) {
                    Ok(p) => p,
                    Err(e) => return Some(Err(VaultError::from(e))),
                };

                let entry = VaultEntry {
                    id,
                    embedding: stored,
                    payload,
                    namespace: ns,
                    content,
                    source_file,
                    added_by,
                    chunk_index,
                    parent_id,
                    created_at,
                    embedder_model_id,
                    dim: resolved_dim,
                };

                Some(Ok((total_score, entry)))
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

    /// Stores the embedder identity, overwriting any previously stored value.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if the database write fails.
    #[must_use = "check the Result to confirm the embedder identity was stored"]
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
    /// Only rows produced by the same model as the query participate: a row whose
    /// resolved `embedder_model_id` ≠ `query_model_id` or resolved `dim` ≠
    /// `query_dim` is skipped before any cosine comparison. A mismatched-dimension
    /// row is therefore excluded from ranking rather than raising an error — a
    /// vault mid-migration keeps returning correct same-model results.
    ///
    /// # Errors
    ///
    /// Returns [`VaultError::Db`] if reading any row fails or [`VaultError::Json`]
    /// if a stored payload cannot be deserialised.
    #[must_use = "the query result is the entire purpose of calling this function"]
    pub fn query(
        &self,
        query_embedding: &[f32],
        k: usize,
        query_model_id: &str,
        query_dim: usize,
    ) -> Result<Vec<QueryResult>, VaultError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, embedding, payload, embedder_model_id, dim FROM vault_entries")?;

        let rows: Vec<QueryRow> = stmt
            .query_map([], |row| {
                Ok(QueryRow {
                    id: row.get(0)?,
                    bytes: row.get(1)?,
                    payload_str: row.get(2)?,
                    model_id: row.get(3)?,
                    dim: row.get(4)?,
                })
            })?
            .collect::<Result<_, rusqlite::Error>>()?;

        let mut scored: Vec<(f32, String, serde_json::Value)> = rows
            .into_iter()
            // Same-model-only: a row from a different model or dimension is never
            // fed to `cosine_sim` and never raises an error — it is simply not a
            // candidate.
            .filter(|row| {
                resolve_model_id(row.model_id.clone()) == query_model_id
                    && resolve_dim(row.dim, row.bytes.len()) == query_dim
            })
            .filter_map(
                |QueryRow {
                     id,
                     bytes,
                     payload_str,
                     ..
                 }| {
                    // Blobs are normally written by `bytemuck::cast_slice::<f32, u8>`
                    // (length always a multiple of 4), but a legacy or corrupted row
                    // may not be — skip it instead of panicking the full scan.
                    let Some(stored) = decode_embedding(&bytes) else {
                        tracing::warn!(id = %id, "vault: skipping query row with malformed embedding blob");
                        return None;
                    };
                    let score = cosine_sim(query_embedding, &stored);
                    let payload: serde_json::Value = match serde_json::from_str(&payload_str) {
                        Ok(p) => p,
                        Err(e) => return Some(Err(VaultError::from(e))),
                    };
                    Some(Ok((score, id, payload)))
                },
            )
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
            .filter_map(|row| {
                let embedder_model_id = resolve_model_id(row.model_id);
                let dim = resolve_dim(row.dim, row.bytes.len());
                let Some(embedding) = decode_embedding(&row.bytes) else {
                    tracing::warn!(id = %row.id, "vault: skipping list row with malformed embedding blob");
                    return None;
                };
                let payload: serde_json::Value = match serde_json::from_str(&row.payload_str) {
                    Ok(p) => p,
                    Err(e) => return Some(Err(VaultError::from(e))),
                };
                Some(Ok(VaultEntry {
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
                }))
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
                created_at   REAL NOT NULL DEFAULT 0.0,
                embedder_model_id TEXT,
                dim          INTEGER
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
            // Per-row model tagging. Nullable on purpose: a NULL marks a legacy
            // row that predates tagging, which reads back as `LEGACY_MODEL_ID`
            // with `dim` derived from the stored BLOB length.
            "ALTER TABLE vault_entries ADD COLUMN embedder_model_id TEXT",
            "ALTER TABLE vault_entries ADD COLUMN dim INTEGER",
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
        let dim = embedding.len();
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
            embedder_model_id: LEGACY_MODEL_ID.to_string(),
            dim,
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

        let results = vault.query(&[1.0_f32, 0.0], 3, LEGACY_MODEL_ID, 2).unwrap();
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

        let results = vault
            .query(&[1.0_f32, 0.0], 10, LEGACY_MODEL_ID, 2)
            .unwrap();
        assert_eq!(results.len(), 3, "must return all entries when k > count");
    }

    #[test]
    fn query_empty_vault() {
        let vault = Vault::open_in_memory().unwrap();
        let results = vault.query(&[1.0_f32, 0.0], 5, LEGACY_MODEL_ID, 2).unwrap();
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

        let results = vault
            .search(&[1.0_f32, 0.0], "agent", "agents", 5, LEGACY_MODEL_ID, 2)
            .unwrap();
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

        let results = vault
            .search(&[1.0_f32, 0.0], "", "default", 2, LEGACY_MODEL_ID, 2)
            .unwrap();
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

    // ── per-row model/dim tagging ─────────────────────────────────────────────

    #[test]
    fn model_id_and_dim_round_trip_through_insert_and_search() {
        let mut vault = Vault::open_in_memory().unwrap();
        let mut e = entry("tagged", vec![1.0_f32, 0.0, 0.0]);
        e.namespace = "ns".to_string();
        e.content = "tagged content".to_string();
        e.embedder_model_id = "minilm-l6-v2".to_string();
        e.dim = 3;
        vault.insert(&e).unwrap();

        let results = vault
            .search(&[1.0_f32, 0.0, 0.0], "tagged", "ns", 5, "minilm-l6-v2", 3)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].embedder_model_id, "minilm-l6-v2");
        assert_eq!(results[0].dim, 3);
    }

    #[test]
    fn legacy_row_reads_back_with_fnv_default_and_blob_derived_dim() {
        let vault = Vault::open_in_memory().unwrap();
        // Insert a row the legacy way: explicit SQL that leaves the new columns
        // NULL, exactly as a pre-migration database would have stored it.
        let embedding = vec![0.5_f32; 128];
        let bytes = bytemuck::cast_slice::<f32, u8>(&embedding);
        vault
            .conn
            .execute(
                "INSERT INTO vault_entries (id, embedding, payload, namespace, content) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["legacy", bytes, "{}", "default", "legacy content"],
            )
            .unwrap();

        let results = vault
            .search(&embedding, "legacy", "default", 5, LEGACY_MODEL_ID, 128)
            .unwrap();
        assert_eq!(
            results.len(),
            1,
            "legacy row must be same-model with FNV id"
        );
        assert_eq!(results[0].embedder_model_id, LEGACY_MODEL_ID);
        assert_eq!(
            results[0].dim, 128,
            "legacy dim must derive from BLOB length / 4"
        );
    }

    // ── same-model-only comparison ────────────────────────────────────────────

    #[test]
    fn search_returns_only_same_model_rows() {
        let mut vault = Vault::open_in_memory().unwrap();

        let mut fnv = entry("fnv", vec![1.0_f32, 0.0]);
        fnv.namespace = "mixed".to_string();
        fnv.content = "shared".to_string();
        fnv.embedder_model_id = LEGACY_MODEL_ID.to_string();
        fnv.dim = 2;
        vault.upsert(&fnv).unwrap();

        // A learned row of a different model AND a different dimension.
        let mut learned = entry("learned", vec![1.0_f32, 0.0, 0.0]);
        learned.namespace = "mixed".to_string();
        learned.content = "shared".to_string();
        learned.embedder_model_id = "minilm".to_string();
        learned.dim = 3;
        vault.upsert(&learned).unwrap();

        // Query under the FNV model: only the FNV row is a candidate; the
        // mismatched-dim learned row is excluded, never compared, never errors.
        let results = vault
            .search(&[1.0_f32, 0.0], "shared", "mixed", 5, LEGACY_MODEL_ID, 2)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "fnv");

        // Query under the learned model: only the learned row is returned.
        let results = vault
            .search(&[1.0_f32, 0.0, 0.0], "shared", "mixed", 5, "minilm", 3)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "learned");
    }

    #[test]
    fn query_excludes_mismatched_dimension_without_error() {
        let mut vault = Vault::open_in_memory().unwrap();

        let mut a = entry("a", vec![1.0_f32, 0.0]);
        a.dim = 2;
        vault.upsert(&a).unwrap();

        let mut b = entry("b", vec![1.0_f32, 0.0, 0.0]);
        b.embedder_model_id = "other".to_string();
        b.dim = 3;
        vault.upsert(&b).unwrap();

        // Querying with a dim-2 FNV vector must NOT raise DimensionMismatch.
        let results = vault.query(&[1.0_f32, 0.0], 5, LEGACY_MODEL_ID, 2).unwrap();
        assert_eq!(results.len(), 1, "only the same-model row is a candidate");
        assert_eq!(results[0].id, "a");
    }

    #[test]
    fn same_model_results_rank_by_descending_hybrid_score() {
        let mut vault = Vault::open_in_memory().unwrap();
        // Regression guard: unchanged hybrid scoring for same-model rows.
        vault.upsert(&entry("exact", vec![1.0_f32, 0.0])).unwrap();
        vault.upsert(&entry("ortho", vec![0.0_f32, 1.0])).unwrap();
        vault.upsert(&entry("close", vec![0.6_f32, 0.8])).unwrap();

        let results = vault
            .search(&[1.0_f32, 0.0], "", "default", 3, LEGACY_MODEL_ID, 2)
            .unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].id, "exact", "highest cosine must rank first");
        assert_eq!(results[2].id, "ortho", "orthogonal must rank last");
    }

    // ── malformed embedding blobs must not panic the retrieval path ───────────

    /// Inserts a row whose embedding BLOB is 3 bytes — not a multiple of 4 — while
    /// tagging it with a `dim`/`model_id` that passes the same-model filter, so
    /// the row reaches the decode step. Before the fix, `bytemuck::cast_slice::<u8,
    /// f32>` on a 3-byte slice panicked, turning any full-scan `query`/`search`
    /// over the store into a store-wide DoS.
    fn insert_corrupt_row(vault: &Vault, id: &str, namespace: &str, dim: i64) {
        // 3 raw bytes: length is not a multiple of 4.
        let corrupt: Vec<u8> = vec![1, 2, 3];
        vault
            .conn
            .execute(
                "INSERT INTO vault_entries \
                 (id, embedding, payload, namespace, content, embedder_model_id, dim) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![id, corrupt, "{}", namespace, "corrupt", LEGACY_MODEL_ID, dim],
            )
            .unwrap();
    }

    #[test]
    fn query_skips_row_with_non_multiple_of_four_blob() {
        let vault = Vault::open_in_memory().unwrap();
        // dim = 1 matches the query dim below, so the row survives the same-model
        // filter and is handed to the decode step (fail-before panicked here).
        insert_corrupt_row(&vault, "corrupt", "default", 1);

        let results = vault
            .query(&[1.0_f32], 5, LEGACY_MODEL_ID, 1)
            .expect("query must skip the malformed row, not panic");
        assert!(
            results.is_empty(),
            "the corrupt row must be skipped, leaving no results"
        );
    }

    #[test]
    fn search_skips_row_with_non_multiple_of_four_blob() {
        let mut vault = Vault::open_in_memory().unwrap();
        insert_corrupt_row(&vault, "corrupt", "ns", 1);

        // A well-formed same-model row so we can confirm the scan continues past
        // the corrupt one rather than aborting the whole call.
        let mut good = entry("good", vec![1.0_f32]);
        good.namespace = "ns".to_string();
        good.content = "good content".to_string();
        good.dim = 1;
        vault.insert(&good).unwrap();

        let results = vault
            .search(&[1.0_f32], "good", "ns", 5, LEGACY_MODEL_ID, 1)
            .expect("search must skip the malformed row, not panic");
        assert_eq!(results.len(), 1, "only the well-formed row is returned");
        assert_eq!(results[0].id, "good");
    }

    #[test]
    fn insert_dedup_scan_skips_corrupt_neighbour() {
        let mut vault = Vault::open_in_memory().unwrap();
        // A corrupt neighbour in the same namespace as the incoming insert. The
        // dedup scan visits every same-namespace row and previously panicked on
        // this blob before it could persist the new entry.
        insert_corrupt_row(&vault, "corrupt", "default", 1);

        let good = entry("good", vec![1.0_f32]);
        vault
            .insert(&good)
            .expect("insert must skip the corrupt dedup neighbour, not panic");

        let results = vault
            .query(&[1.0_f32], 5, LEGACY_MODEL_ID, 1)
            .expect("query after insert must not panic");
        assert_eq!(results.len(), 1, "the freshly inserted row is present");
        assert_eq!(results[0].id, "good");
    }
}
